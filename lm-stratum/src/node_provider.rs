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
//! matching draw (doc 10 §10.8,. Without the flags the
//! node's template passes through untouched — normal-mode mining.

use crate::provider::{BlockOutcome, JobTemplate, TemplateProvider};
use crate::rpc::RpcClient;
use crate::vardiff::target_for_work;
use lm_pow::pow_hash;
use sha2::{Digest, Sha256};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How often the provider re-polls the node for a fresh template.
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(2);

/// Initial per-connection share work in node mode: the template's share
/// target starts at the easier of the network target and this work level,
/// so a fresh (or very hard) chain still produces shares immediately —
/// dashboard liveness and vardiff convergence both need accepted shares,
/// and vardiff only retargets after 8 of them. Shares easier than the
/// network target are still just shares; the server submits a block only
/// when the digest meets `net_target`.
pub const DEFAULT_START_WORK: u64 = 1 << 16;

/// One retained template: merkle -> tx set + its claims (all 0 for
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
    /// Faucet seed-fund output claimed by this template (0 = none).
    pub faucet: u64,
    /// Dev seed-fund output claimed by this template (0 = none).
    pub dev: u64,
    /// Consensus Luckynode Draw claim spliced into this template's
    /// coinbase (0 = none): pays the epoch's consensus winner, a third
    /// party — never miner-owed (doc 06 §6.7).
    pub prize_claim: u64,
    /// Winner address the claim above pays (empty when no claim).
    pub prize_winner: String,
    /// Advisory prize-fund overflow for this height (base units): the
    /// jackpot-levy slice overflowing the reserve cap.
    pub prize: u64,
    /// Normal net value of the template's coinbase (the pool's receipt).
    pub coinbase: u64,
    /// This template is a pity-timer claim (node drought at/over the
    /// limit): submittable on ANY digest, floor claim, node pays tier 0.
    pub pity: bool,
}

#[derive(Debug, Clone)]
struct TemplateState {
    template: JobTemplate,
    /// All transactions, coinbase first, in template order.
    txs: Vec<Vec<u8>>,
    /// Recent (merkle -> tx set, claims) ring. The header's merkle field
    /// commits to the tx set, so a submission must be assembled with the
    /// tx set THAT MATCHED THE JOB'S HEADER — not the latest template
    /// (found in testing: a 2s refresh between grind and submit swapped the
    /// coinbase and made the submitted merkle disagree with its own
    /// header).
    recent: std::collections::VecDeque<RecentTemplate>,
    /// Bonus claimed by this template's coinbase (0 = normal template).
    bonus: u64,
    /// Pot claimed by this template's coinbase (0 = normal template).
    pot_claim: u64,
    /// Faucet seed-fund output claimed by this template (0 = none).
    faucet_claim: u64,
    /// Dev seed-fund output claimed by this template (0 = none).
    dev_claim: u64,
    /// Consensus draw claim spliced into this template's coinbase
    /// (0 = none; third-party winner, never miner-owed).
    prize_claim: u64,
    /// Winner address the claim above pays (empty when no claim).
    prize_winner: String,
    /// Advisory prize-fund overflow for the current template's height
    /// (base units; credited to the pool's prize ledger on accept).
    prize: u64,
    /// Normal net value of the current template's coinbase (parsed from the
    /// node's own template bytes — what an accepted block pays the pool).
    coinbase: u64,
    /// Current template is a pity-timer claim (any digest submittable).
    pity: bool,
    fetched_at: Instant,
}

/// Merkle field offset in the Addendum A header layout.
const MERKLE_RANGE: std::ops::Range<usize> = 36..68;

fn header_merkle(header: &[u8]) -> Option<[u8; 32]> {
    header.get(MERKLE_RANGE).and_then(|m| m.try_into().ok())
}

/// Complete claim configuration for a node-backed provider. Passed at
/// construction so the FIRST template already carries the resolved
/// divisors and faucet/dev decisions — building with defaults and
/// reconfiguring after would serve one wrong-claim template until the
/// first refresh (up to 2 s of miners grinding claims the node would
/// reject, or missing seed drips it advertised).
#[derive(Clone, Debug)]
pub struct ProviderConfig {
    pub claim_jackpots: bool,
    pub claim_pot: bool,
    pub claim_faucet: bool,
    pub claim_dev: bool,
    /// Splice the node's consensus draw claim (doc 06 §6.7) into every
    /// template it advertises. No-op on chains whose nodes predate the
    /// unit-2 GBT fields.
    pub claim_prize: bool,
    pub jackpot_divisor: u64,
    pub pot_divisor: u64,
    /// Landed-but-immature faucet claims restored from the state file,
    /// so the first template's top-up math already accounts for them.
    pub restored_unmatured: Vec<(u64, u64)>,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            claim_jackpots: true,
            claim_pot: true,
            claim_faucet: false,
            claim_dev: false,
            claim_prize: true,
            jackpot_divisor: 100,
            pot_divisor: 1000,
            restored_unmatured: Vec::new(),
        }
    }
}

impl ProviderConfig {
    /// Faucet/dev claims off (the pool binary turns them on explicitly).
    pub fn new(
        claim_jackpots: bool,
        claim_pot: bool,
        claim_faucet: bool,
        claim_dev: bool,
        claim_prize: bool,
        jackpot_divisor: u64,
        pot_divisor: u64,
    ) -> Self {
        Self {
            claim_jackpots,
            claim_pot,
            claim_faucet,
            claim_dev,
            claim_prize,
            jackpot_divisor: jackpot_divisor.max(1),
            pot_divisor: pot_divisor.max(1),
            restored_unmatured: Vec::new(),
        }
    }

    pub fn with_restored_unmatured(mut self, v: Vec<(u64, u64)>) -> Self {
        self.restored_unmatured = v;
        self
    }
}

/// Decide the seed-fund legs (faucet top-up, dev cadence) for one
/// template. Shared by the constructor (first template) and the refresh
/// loop so both apply identical policy. Prunes matured entries from the
/// unmatured map as a side effect.
fn decide_seed_claim<F>(
    faucet_balance: &F,
    gbt: &serde_json::Value,
    height: u64,
    claim_faucet: bool,
    claim_dev: bool,
    unmatured: &mut std::collections::BTreeMap<u64, u64>,
) -> SeedClaim
where
    F: Fn(&str) -> Option<u64>,
{
    let (faucet_drip, faucet_script, dev_drip, dev_script, dev_epoch) =
        parse_seed_fields(gbt);
    let faucet_need = if claim_faucet && faucet_drip > 0 && !faucet_script.is_empty() {
        match gbt
            .get("faucetaddress")
            .and_then(|v| v.as_str())
            .and_then(faucet_balance)
        {
            Some(balance) => {
                unmatured.retain(|h, _| h.saturating_add(FAUCET_MATURITY) > height);
                // Pending = LANDED-but-immature claims only (recorded
                // at accept, never at template-build: an unlanded
                // template must not suppress future top-ups).
                let pending = unmatured_total(unmatured, height, FAUCET_MATURITY);
                let need = faucet_topup_amount(
                    faucet_drip,
                    balance.saturating_add(pending),
                    FAUCET_TOPUP_TARGET,
                );
                if need > 0 {
                    eprintln!(
                        "faucet claim: height={} drip={} wallet={} pending={} claiming={}",
                        height, faucet_drip, balance, pending, need
                    );
                }
                need
            }
            None => 0,
        }
    } else {
        0
    };
    // Dev policy is cadence-driven: claim the full drip on
    // dev-epoch heights when enabled.
    let dev_need = if claim_dev && dev_epoch && dev_drip > 0 && !dev_script.is_empty() {
        dev_drip
    } else {
        0
    };
    SeedClaim {
        faucet: faucet_need,
        faucet_script,
        dev: dev_need,
        dev_script,
    }
}

/// Provider backed by a running node's JSON-RPC.
pub struct NodeProvider {
    rpc: RpcClient,
    state: Mutex<TemplateState>,
    accepted: Mutex<u64>,
    rejected: Mutex<u64>,
    /// Serve jackpot-bonus templates on pre-committed jackpot heights
    /// (coinbase claims the node's exact advertised bonus). Every find on
    /// such a template is submittable — no withholding (doc 06 rev 1.2).
    claim_jackpots: bool,
    /// Serve pot templates on pre-committed pot heights (exact advertised
    /// splash, tiers and pity included). Every find submittable.
    claim_pot: bool,
    /// Pot draw divisor: `draw_digest < 2^256/divisor` (1000 = frozen
    /// spec). Regtest may lower it (node `-lucky-pot-divisor`) to prove
    /// the claim path fast; the local recompute must match the node.
    pot_divisor: u64,
    /// Jackpot draw divisor: `draw_digest < 2^256/divisor` (100 = frozen
    /// spec; regtest `-lucky-jackpot-divisor` may lower it — the local
    /// recompute must match the node).
    jackpot_divisor: u64,
    /// Claim faucet seed-fund outputs (top-up policy: only what the
    /// faucet wallet needs, up to the node's advertised drip).
    claim_faucet: bool,
    /// Claim dev seed-fund outputs on dev-epoch heights (up to the
    /// node's advertised drip). The cadence is consensus (every
    /// DEV_EPOCH-th block); the pool only decides on/off.
    claim_dev: bool,
    /// Splice the consensus draw claim (doc 06 §6.7).
    claim_prize: bool,
    /// Faucet amounts this pool claimed per height (height -> amount).
    /// Landed-but-immature claims count toward the top-up target (the
    /// node wallet can't see them via listunspent for 100 blocks);
    /// pruned as they mature. Without this the pool would claim the max
    /// drip every block until maturity — draining the seed in ~100
    /// blocks regardless of demand.
    faucet_unmatured: Mutex<std::collections::BTreeMap<u64, u64>>,
}

impl NodeProvider {
    /// Connect and fetch the first template; fails fast if the node is
    /// unreachable or returns something unmappable. Default config:
    /// jackpot + pot claims on, faucet/dev off, frozen divisors, no
    /// restored in-flight claims.
    pub fn connect(addr: &str, user: &str, pass: &str) -> Result<Self, String> {
        Self::connect_with_config(addr, user, pass, ProviderConfig::default())
    }

    /// As [`connect`], with the full claim configuration applied BEFORE
    /// the first template is built — every template this provider ever
    /// serves, including the first, uses the resolved divisors and the
    /// faucet/dev decisions (the old construct-then-configure sequence
    /// served one template with hardcoded divisors and no seed claims).
    pub fn connect_with_config(
        addr: &str,
        user: &str,
        pass: &str,
        cfg: ProviderConfig,
    ) -> Result<Self, String> {
        let rpc = RpcClient::new(addr, user, pass);
        let gbt = rpc.call(
            "getblocktemplate",
            serde_json::json!([{"capabilities": []}]),
        )?;
        let height = gbt.get("height").and_then(|v| v.as_u64()).unwrap_or(0);
        let prize = parse_prize_inflow(&gbt);
        let mut unmatured: std::collections::BTreeMap<u64, u64> =
            cfg.restored_unmatured.into_iter().collect();
        let seed = decide_seed_claim(
            &|addr| faucet_wallet_balance(&rpc, addr),
            &gbt,
            height,
            cfg.claim_faucet,
            cfg.claim_dev,
            &mut unmatured,
        );
        let (tpl, txs, bonus, pot, coinbase, pity, faucet, dev, prize_splice) = build_template(
            &gbt,
            cfg.claim_jackpots,
            cfg.claim_pot,
            cfg.jackpot_divisor.max(1),
            cfg.pot_divisor.max(1),
            &seed,
            cfg.claim_prize,
        )?;
        let mut recent = std::collections::VecDeque::new();
        if let Some(m) = header_merkle(&tpl.header) {
            recent.push_front(RecentTemplate {
                merkle: m,
                txs: txs.clone(),
                bonus,
                pot,
                faucet,
                dev,
                coinbase,
                pity,
                prize,
                prize_claim: prize_splice.amount,
                prize_winner: prize_splice.winner.clone(),
            });
        }
        Ok(Self {
            rpc,
            state: Mutex::new(TemplateState {
                template: tpl,
                txs,
                recent,
                bonus,
                pot_claim: pot,
                faucet_claim: faucet,
                dev_claim: dev,
                prize_claim: prize_splice.amount,
                prize_winner: prize_splice.winner,
                coinbase,
                pity,
                prize,
                fetched_at: Instant::now(),
            }),
            accepted: Mutex::new(0),
            rejected: Mutex::new(0),
            claim_jackpots: cfg.claim_jackpots,
            claim_pot: cfg.claim_pot,
            claim_faucet: cfg.claim_faucet,
            claim_dev: cfg.claim_dev,
            claim_prize: cfg.claim_prize,
            faucet_unmatured: Mutex::new(unmatured),
            pot_divisor: cfg.pot_divisor.max(1),
            jackpot_divisor: cfg.jackpot_divisor.max(1),
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
        let claim_faucet = self.claim_faucet;
        let claim_dev = self.claim_dev;
        let claim_prize = self.claim_prize;
        let jackpot_divisor = self.jackpot_divisor;
        let pot_divisor = self.pot_divisor;
        match self
            .rpc
            .call(
                "getblocktemplate",
                serde_json::json!([{"capabilities": []}]),
            )
            .and_then(|gbt| {
                let prize = parse_prize_inflow(&gbt);
                // Seed-fund claims (internal/20): decided by the shared
                // helper so the constructor's first template and every
                // refresh apply identical policy.
                let height = gbt.get("height").and_then(|v| v.as_u64()).unwrap_or(0);
                let mut unm = self.faucet_unmatured.lock().expect("unmatured");
                let seed = decide_seed_claim(
                    &|addr| faucet_wallet_balance(&self.rpc, addr),
                    &gbt,
                    height,
                    claim_faucet,
                    claim_dev,
                    &mut unm,
                );
                drop(unm);
                build_template(&gbt, claim_jackpots, claim_pot, jackpot_divisor, pot_divisor, &seed, claim_prize)
                    .map(|bt| (bt, prize))
            }) {
            Ok(((tpl, txs, bonus, pot, coinbase, pity, faucet, dev, prize_splice), prize)) => {
                let mut st = self.state.lock().expect("state");
                if let Some(m) = header_merkle(&tpl.header) {
                    st.recent.push_front(RecentTemplate {
                        merkle: m,
                        txs: txs.clone(),
                        bonus,
                        pot,
                        faucet,
                        dev,
                        coinbase,
                        pity,
                        prize,
                        prize_claim: prize_splice.amount,
                        prize_winner: prize_splice.winner.clone(),
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
                st.faucet_claim = faucet;
                st.dev_claim = dev;
                st.coinbase = coinbase;
                st.pity = pity;
                st.prize = prize;
                st.prize_claim = prize_splice.amount;
                st.prize_winner = prize_splice.winner;
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

    fn faucet_unmatured_snapshot(&self) -> Vec<(u64, u64)> {
        self.faucet_unmatured
            .lock()
            .expect("unmatured")
            .iter()
            .map(|(h, a)| (*h, *a))
            .collect()
    }

    fn restore_faucet_unmatured(&self, claimed: Vec<(u64, u64)>) {
        let mut unm = self.faucet_unmatured.lock().expect("unmatured");
        unm.clear();
        for (h, a) in claimed {
            unm.insert(h, a);
        }
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
                    pot: 0,
                    faucet: 0,
                    dev: 0,
                    prize_inflow: 0,
                    prize_claim: 0,
                    prize_winner: String::new(),
                    reserve: 0,
                    coinbase: 0,
                };
            }
        };
        // Locate the template this header belongs to (current or ring) —
        // assembly AND gating both need ITS claims, not the latest
        // template's. Its normal coinbase value rides along for payout
        // accounting.
        let (txs, bonus, pot, faucet, dev, coinbase, _pity, prize, prize_claim, prize_winner) = {
            let st = self.state.lock().expect("state");
            if header_merkle(&st.template.header) == Some(merkle) {
                (
                    st.txs.clone(),
                    st.bonus,
                    st.pot_claim,
                    st.faucet_claim,
                    st.dev_claim,
                    st.coinbase,
                    st.pity,
                    st.prize,
                    st.prize_claim,
                    st.prize_winner.clone(),
                )
            } else {
                match st.recent.iter().find(|r| r.merkle == merkle) {
                    Some(r) => (
                        r.txs.clone(),
                        r.bonus,
                        r.pot,
                        r.faucet,
                        r.dev,
                        r.coinbase,
                        r.pity,
                        r.prize,
                        r.prize_claim,
                        r.prize_winner.clone(),
                    ),
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
                            pot: 0,
                            faucet: 0,
                            dev: 0,
                            prize_inflow: 0,
                            prize_claim: 0,
                            prize_winner: String::new(),
                            reserve: 0,
                            coinbase: 0,
                        };
                    }
                }
            }
        };
        // No pre-submit gate: draws are pre-committed per height (doc 06
        // rev 1.2), so a claim template's coinbase is valid for EVERY
        // find. The template's claims ride along for payout accounting.
        let block_hex = assemble_block(header, nonce, &txs);
        let result = self
            .rpc
            .call("submitblock", serde_json::json!([hex_of(&block_hex)]));
        let accepted = matches!(&result, Ok(v) if v.is_null());
        if accepted {
            *self.accepted.lock().expect("accepted") += 1;
            // Record LANDED faucet claims for the top-up policy (never
            // intentions: an unlanded template must not suppress future
            // top-ups). Insert-or-max per height: reorgs can only lower
            // the true pending total, never raise it past a landed claim.
            if faucet > 0 {
                let mut unm = self.faucet_unmatured.lock().expect("unmatured");
                unm.entry(height)
                    .and_modify(|v| *v = (*v).max(faucet))
                    .or_insert(faucet);
            }
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
            // Report what the accepted coinbase actually claimed: the
            // server splits the game funds across the PPLNS window; seed
            // outputs are third-party (faucet/dev) and ride along for
            // the record only — never miner-owed.
            bonus: if accepted { bonus } else { 0 },
            pot: if accepted { pot } else { 0 },
            faucet: if accepted { faucet } else { 0 },
            dev: if accepted { dev } else { 0 },
            // Prize credit rides on the accepted template's advisory
            // (the node computed it with the same consensus math). The
            // consensus draw claim is a third-party output to the
            // winner (faucet/dev-style record, never miner-owed).
            prize_inflow: if accepted { prize } else { 0 },
            prize_claim: if accepted { prize_claim } else { 0 },
            prize_winner: if accepted { prize_winner } else { String::new() },
            reserve: 0,
            coinbase: if accepted { coinbase } else { 0 },
        }
    }
}

/// Advisory prize-fund overflow for the template's height (base units):
/// the jackpot-levy slice that overflows the reserve cap and funds the
/// prize fund / Luckynode Draw budgets (doc 06 §6.2,.
/// Nodes predating the field read as 0 (safe: no credit).
#[must_use]
pub fn parse_prize_inflow(gbt: &serde_json::Value) -> u64 {
    gbt.get("prizeoverflow").and_then(|v| v.as_u64()).unwrap_or(0)
}

/// Consensus Luckynode Draw claim for one template (doc 06 §6.7): the
/// epoch, validity, remaining budget, winner address (display/logging
/// only — the script is the splice target), and winner script bytes.
/// The pool splices `remaining` to `script` verbatim: the node
/// recomputed the draw and validates the exact directed sum at submit
/// time, so a disagreement refuses the block loudly (`bad-prize-claim`)
/// instead of minting bad money. Old nodes (no fields) read as
/// invalid → no claim (safe: templates pass through untouched).
#[must_use]
pub fn parse_prize_draw(gbt: &serde_json::Value) -> (u64, bool, u64, String, Vec<u8>) {
    let epoch = gbt.get("prizedrawepoch").and_then(|v| v.as_u64()).unwrap_or(0);
    let valid = gbt.get("prizedrawvalid").and_then(|v| v.as_bool()).unwrap_or(false);
    let remaining = gbt
        .get("prizedrawremaining")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let winner = gbt
        .get("prizedrawwinner")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let script = gbt
        .get("prizedrawscript")
        .and_then(|v| v.as_str())
        .and_then(|s| hex_bytes(s).ok())
        .unwrap_or_default();
    (epoch, valid, remaining, winner, script)
}

// The pre-committed draw math lives in `lm-pow` (consensus-adjacent
// code belongs beside the frozen KAT vectors); re-exported here so the
// provider and its callers are unchanged.
pub use lm_pow::draw::{draw_class, precommitted_digest, DrawClass};

/// Parse a `getblocktemplate` result into (JobTemplate, all tx bytes with the coinbase first).
/// Pure and testable against a frozen fixture.
pub fn parse_gbt(gbt: &serde_json::Value) -> Result<(JobTemplate, Vec<Vec<u8>>), String> {
    let obj = gbt.as_object().ok_or("gbt: not an object")?;
    let version = obj
        .get("version")
        .and_then(|v| v.as_i64())
        .ok_or("gbt: version")? as u32;
    let prev = obj
        .get("previousblockhash")
        .and_then(|v| v.as_str())
        .ok_or("gbt: prev")?;
    let sapling = obj
        .get("finalsaplingroothash")
        .and_then(|v| v.as_str())
        .ok_or("gbt: finalsaplingroothash")?;
    let time = obj
        .get("curtime")
        .and_then(|v| v.as_u64())
        .ok_or("gbt: curtime")? as u32;
    let bits_hex = obj
        .get("bits")
        .and_then(|v| v.as_str())
        .ok_or("gbt: bits")?;
    let bits = u32::from_str_radix(bits_hex, 16).map_err(|_| "gbt: bits hex")?;
    let height = obj
        .get("height")
        .and_then(|v| v.as_u64())
        .ok_or("gbt: height")?;
    let target = parse_target(
        obj.get("target")
            .and_then(|v| v.as_str())
            .ok_or("gbt: target")?,
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
    cross_check(
        "coinbasetxn.hash",
        obj.get("coinbasetxn").and_then(|c| c.get("hash")),
        &coinbase_txid,
    )?;
    let mut txs = vec![coinbase];
    let mut txids = vec![coinbase_txid];
    let mempool = obj
        .get("transactions")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    for (i, tx) in mempool.iter().enumerate() {
        let data = hex_decode(
            tx.get("data")
                .and_then(|v| v.as_str())
                .ok_or_else(|| format!("gbt: transactions[{i}].data"))?,
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

    // Share target: never harder than the network target, and never
    // harder than DEFAULT_START_WORK — a fresh chain's net target can be
    // minutes-per-share, which starves vardiff (needs accepted shares to
    // adapt) and leaves the dashboard at 0 H/s. Bigger target = easier.
    let share_target = std::cmp::max(target, target_for_work(DEFAULT_START_WORK));

    Ok((
        JobTemplate {
            header,
            height,
            pool_target: share_target,
            net_target: target,
            // Prospective prize inflow for this height, from the
            // node's own advisory — the pool credits it when the height
            // lands, whoever found the block.
            prize: parse_prize_inflow(gbt),
        },
        txs,
    ))
}

/// Seed-fund claim for one template (internal/20): the pool-decided
/// amounts against the node's advertised allowances, plus the pinned
/// scripts to splice verbatim (node is source of truth — the pool never
/// invents a destination).
#[derive(Debug, Clone, Default)]
pub struct SeedClaim {
    /// Faucet output value (0 = none).
    pub faucet: u64,
    /// Pinned faucet script bytes (from GBT `faucetscript`).
    pub faucet_script: Vec<u8>,
    /// Dev output value (0 = none).
    pub dev: u64,
    /// Pinned dev script bytes (from GBT `devscript`).
    pub dev_script: Vec<u8>,
}

/// Faucet top-up target (base units): the pool claims only what the
/// faucet wallet needs to reach this, up to the advertised drip.
/// Pool policy, not consensus — 1,000 tLKYL funds ~100 drips at the
/// 10-coin faucet amount.
pub const FAUCET_TOPUP_TARGET: u64 = 100_000_000_000;

/// Coinbase maturity for unmatured-claim accounting (consensus default;
/// regtest may override it node-side, but the pool only needs the
/// production value — regtest proofs spend instantly).
pub const FAUCET_MATURITY: u64 = 100;

/// Hex string -> bytes (GBT scripts). Rejects odd lengths and non-hex.
fn hex_bytes(s: &str) -> Result<Vec<u8>, String> {
    if !s.len().is_multiple_of(2) {
        return Err("odd-length hex".to_string());
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| "bad hex".to_string()))
        .collect()
}

/// Parse the seed-fund GBT advisories: allowances + pinned scripts.
/// Missing fields read as 0/empty (old nodes: no claims — safe).
#[must_use]
pub fn parse_seed_fields(gbt: &serde_json::Value) -> (u64, Vec<u8>, u64, Vec<u8>, bool) {
    let faucet_drip = gbt.get("faucetdrip").and_then(|v| v.as_u64()).unwrap_or(0);
    let faucet_script = gbt
        .get("faucetscript")
        .and_then(|v| v.as_str())
        .and_then(|s| hex_bytes(s).ok())
        .unwrap_or_default();
    let dev_drip = gbt.get("devdrip").and_then(|v| v.as_u64()).unwrap_or(0);
    let dev_script = gbt
        .get("devscript")
        .and_then(|v| v.as_str())
        .and_then(|s| hex_bytes(s).ok())
        .unwrap_or_default();
    let dev_epoch = gbt.get("devepoch").and_then(|v| v.as_bool()).unwrap_or(false);
    (faucet_drip, faucet_script, dev_drip, dev_script, dev_epoch)
}

/// Faucet top-up math (pure, tested): claim only the shortfall to the
/// target, capped by the node's advertised drip. Zero when the wallet
/// is already at/above target.
#[must_use]
pub fn faucet_topup_amount(drip: u64, wallet: u64, target: u64) -> u64 {
    if drip == 0 || wallet >= target {
        return 0;
    }
    (target - wallet).min(drip)
}

/// Unmatured faucet claims (height -> amount): the pool's own claims
/// that have landed but not yet matured. `listunspent` hides immature
/// coinbases, so without this the wallet would read 0 for 100 blocks
/// and the pool would claim the max drip every block — draining the
/// whole seed in ~100 blocks regardless of demand. Returns the pending
/// total; drops entries that have matured as of `height`.
#[must_use]
pub fn unmatured_total(
    claims: &std::collections::BTreeMap<u64, u64>,
    height: u64,
    maturity: u64,
) -> u64 {
    claims
        .iter()
        .filter(|(h, _)| **h + maturity > height)
        .map(|(_, a)| *a)
        .sum()
}

/// Faucet wallet balance (base units) for the top-up policy: sum of
/// spendable UTXOs paying the pinned faucet address. `None` = unknown
/// (RPC failed) — the caller claims nothing rather than guessing.
fn faucet_wallet_balance(rpc: &RpcClient, faucet_address: &str) -> Option<u64> {
    if faucet_address.is_empty() {
        return None;
    }
    let utxos = rpc
        .call("listunspent", serde_json::json!([0, 9999999, [faucet_address]]))
        .ok()?;
    let arr = utxos.as_array()?;
    let mut total: u64 = 0;
    for u in arr {
        // Prefer exact integer satoshis; fall back to the decimal amount.
        if let Some(sats) = u.get("satoshis").and_then(|v| v.as_u64()) {
            total = total.saturating_add(sats);
        } else if let Some(coins) = u.get("amount").and_then(|v| v.as_f64()) {
            total = total.saturating_add((coins * 100_000_000.0).round().max(0.0) as u64);
        }
    }
    Some(total)
}

/// Built template + its claims: (template, txs, claimed bonus, claimed
/// pot, normal coinbase value, pity claim, claimed faucet, claimed dev,
/// prize splice). The 9-tuple replaced the old 8-tuple when the
/// consensus draw claim landed; a struct may win if it grows again.
pub type BuiltTemplate = (
    JobTemplate,
    Vec<Vec<u8>>,
    u64,
    u64,
    u64,
    bool,
    u64,
    u64,
    PrizeSplice,
);

/// Luckynode Draw claim spliced into a template (doc 06 §6.7): the
/// node's verbatim remaining budget plus the winner it pays. Default =
/// `(0, "")`, no claim.
#[derive(Debug, Clone, Default)]
pub struct PrizeSplice {
    /// Consensus-claimed sats paying the winner (third party, never
    /// miner-owed).
    pub amount: u64,
    /// Winner address (display/logging; the script went into the
    /// coinbase, not this string).
    pub winner: String,
}

/// GBT -> [`BuiltTemplate`].
///
/// With `claim_jackpots` and a nonzero advisory `jackpotbonus`, the
/// coinbase gains an output of that amount (same script as its last
/// output). With `claim_pot` and a nonzero advisory `potcredited`, it
/// gains the tier-A floor of the credited pot (`floor(credited/10)`) —
/// the claim any pot event covers, so a floor-claim template is
/// submittable on every tier. Seed claims (`seed.faucet`/`seed.dev`)
/// splice outputs paying the pinned scripts verbatim. The header merkle
/// is recomputed after the splices. Without claims (or with zero
/// amounts) the node's template passes through untouched. The normal
/// coinbase value is parsed from the node's own template bytes (before
/// splicing) — what an accepted block pays the pool.
pub fn build_template(
    gbt: &serde_json::Value,
    claim_jackpots: bool,
    claim_pot: bool,
    jackpot_divisor: u64,
    pot_divisor: u64,
    seed: &SeedClaim,
    claim_prize: bool,
) -> Result<BuiltTemplate, String> {
    let (mut tpl, mut txs) = parse_gbt(gbt)?;
    let height = tpl.height;
    // Local pre-committed draw class (doc 06 rev 1.2): recomputed from the
    // template's own parent hash + height, byte-for-byte the node's rule.
    // Claiming is gated on THIS, not on the advisory — a mismatched node
    // version degrades to "no claim" (safe) instead of "invalid blocks".
    let prev_internal: [u8; 32] = tpl.header[4..36]
        .try_into()
        .map_err(|_| "template header too short for prev hash".to_string())?;
    let digest = precommitted_digest(&prev_internal, height);
    let class = draw_class(&digest, jackpot_divisor, pot_divisor);
    // Exact advertised amounts (the node computed the same class): the
    // jackpot bonus this height can claim and the pot splash (tier or
    // pity floor). The pool claims them verbatim.
    let adv_bonus = gbt
        .get("jackpotbonus")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let adv_pot = gbt.get("potcredited").and_then(|v| v.as_u64()).unwrap_or(0);
    // Net normal value from the node's own bytes, before splicing.
    let coinbase = crate::tx::coinbase_outputs_total(&txs[0])
        .map_err(|e| format!("coinbase total for payout accounting: {e:?}"))?;
    // Claim iff the local draw says this height qualifies AND the node
    // advertised a nonzero exact amount. Pity splashes are inside
    // `potcredited` (the node forces the floor at drought >= limit), so a
    // pity-due height claims whether or not it is a digest event.
    let drought = gbt.get("potdrought").and_then(|v| v.as_u64()).unwrap_or(0);
    let drought_limit = gbt
        .get("potdroughtlimit")
        .and_then(|v| v.as_u64())
        .unwrap_or(u64::MAX);
    let pity_due = drought >= drought_limit;
    let claimed_bonus = if claim_jackpots && class.jackpot { adv_bonus } else { 0 };
    let claimed_pot = if claim_pot && (class.pot_event || pity_due) {
        adv_pot
    } else {
        0
    };
    // Seed-fund claims ride the pool-decided amounts (already capped
    // against the node's advisories by the caller): an empty pinned
    // script means the leg is disabled, so a nonzero amount with no
    // script — a node disagreeing with the pool about the pins — claims
    // nothing instead of an invalid block.
    let claimed_faucet = if seed.faucet > 0 && !seed.faucet_script.is_empty() {
        seed.faucet
    } else {
        0
    };
    let claimed_dev = if seed.dev > 0 && !seed.dev_script.is_empty() {
        seed.dev
    } else {
        0
    };
    // Consensus prize draw claim (doc 06 §6.7): the node's exact
    // remaining budget for this epoch's winner, spliced VERBATIM to the
    // winner's script. The node recomputed the draw and enforces the
    // exact directed sum, so the pool needs no local draw check. Skipped
    // when ANY existing output already pays that script — the sum would
    // drift and the block would be refused (same builder rule as the
    // internal miner; the budget stays for another block).
    let (_draw_epoch, draw_valid, draw_remaining, draw_winner, draw_script) =
        parse_prize_draw(gbt);
    let mut prize = PrizeSplice::default();
    if claim_prize && draw_valid && draw_remaining > 0 && !draw_script.is_empty() {
        let scripts = crate::tx::output_scripts(&txs[0])
            .map_err(|e| format!("prize collision scan: {e:?}"))?;
        if scripts.contains(&draw_script) {
            static LAST_SKIP_LOG: std::sync::atomic::AtomicU64 =
                std::sync::atomic::AtomicU64::new(u64::MAX);
            if LAST_SKIP_LOG.swap(height, std::sync::atomic::Ordering::Relaxed) != height {
                eprintln!(
                    "prize claim skipped at height {height}: winner script already paid by the template coinbase (builder rule doc 06 §6.7.3)"
                );
            }
        } else {
            prize = PrizeSplice {
                amount: draw_remaining,
                winner: draw_winner,
            };
        }
    }
    if (claimed_bonus == 0
        && claimed_pot == 0
        && claimed_faucet == 0
        && claimed_dev == 0
        && prize.amount == 0)
        || txs.is_empty()
    {
        return Ok((tpl, txs, 0, 0, coinbase, false, 0, 0, PrizeSplice::default()));
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
    if claimed_faucet > 0 {
        txs[0] = crate::tx::append_output(&txs[0], claimed_faucet as i64, &seed.faucet_script)
            .map_err(|e| format!("faucet splice: {e:?}"))?;
    }
    if claimed_dev > 0 {
        txs[0] = crate::tx::append_output(&txs[0], claimed_dev as i64, &seed.dev_script)
            .map_err(|e| format!("dev splice: {e:?}"))?;
    }
    if prize.amount > 0 {
        txs[0] = crate::tx::append_output(&txs[0], prize.amount as i64, &draw_script)
            .map_err(|e| format!("prize splice: {e:?}"))?;
    }
    let new_root = merkle_root(&txs.iter().map(|t| txid(t)).collect::<Vec<_>>())?;
    tpl.header[MERKLE_RANGE].copy_from_slice(&new_root);
    // `pity` in the built template means "the claimed splash came from the
    // pity timer" — informational (there is no submission gate anymore).
    let pity = claimed_pot > 0 && !class.pot_event;
    Ok((tpl, txs, claimed_bonus, claimed_pot, coinbase, pity, claimed_faucet, claimed_dev, prize))
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
fn cross_check(
    field: &str,
    stated: Option<&serde_json::Value>,
    computed: &[u8; 32],
) -> Result<(), String> {
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
/// LuckyHash compare digests as big-endian byte strings.
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
        // target big-endian as returned; the share target starts easier
        // than net so even a hard/fresh chain yields shares (vardiff then
        // adapts per connection)
        assert_eq!(&tpl.net_target[..4], &[0x00, 0x00, 0xff, 0xff]);
        assert_eq!(tpl.pool_target, target_for_work(DEFAULT_START_WORK));
        assert!(tpl.pool_target >= tpl.net_target);
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
        assert_eq!(
            rev(&txid(&tx1)),
            "f70cb6b3f8fb57ab4e21c7475db89f601e9d649cdaa312d5ff6bc77505edc388"
        );
        assert_eq!(
            rev(&txid(&tx2)),
            "e1c6db6a573fdf2008af9f0d7d0a274b67e1aae5788b2af673e1da25c40ce3c7"
        );
        // Golden 3-leaf tree (frozen coinbase from the height-5 fixture).
        let gbt: serde_json::Value = serde_json::from_str(GBT_FIXTURE).expect("json");
        let cb = hex_decode(gbt["coinbasetxn"]["data"].as_str().expect("cb data")).expect("cb");
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
        assert_eq!(
            &tpl.header[36..68],
            &merkle_root(&[txid(&txs[0]), txid(&txs[1]), txid(&txs[2])]).expect("tree")[..]
        );
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
        let (t0, tx0, b0, p0, c0, _, _, _, _) =
            build_template(&gbt, false, false, 100, 1000, &SeedClaim::default(), false).expect("passthrough");
        let (t1, tx1) = parse_gbt(&gbt).expect("parse");
        assert_eq!((b0, p0), (0, 0));
        assert_eq!(t0.header, t1.header);
        assert_eq!(tx0, tx1);
        // Normal coinbase value from the fixture's single vout.
        assert_eq!(c0, 0x01_22a4c3);
        // claim=true on an all-draw height (regtest divisor 1): the coinbase
        // gains a vout, merkle patched to the new txid.
        let (t2, tx2, b2, p2, c2, _, _, _, _) =
            build_template(&gbt, true, false, 1, 1000, &SeedClaim::default(), false).expect("bonus template");
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
        let (_, _, b3, p3, c3, _, _, _, _) =
            build_template(&gbt, true, false, 1, 1000, &SeedClaim::default(), false).expect("zero bonus");
        assert_eq!((b3, p3), (0, 0));
        assert_eq!(c3, 0x01_22a4c3);
    }

    #[test]
    fn pot_template_splices_exact_advertised_splash() {
        let mut gbt: serde_json::Value = serde_json::from_str(GBT_FIXTURE).expect("json");
        // The node's `potcredited` is the EXACT splash (doc 06 rev 1.2):
        // the pool splices it verbatim — no tier math pool-side.
        gbt["potcredited"] = serde_json::json!(2_500_000u64);
        gbt["potdrought"] = serde_json::json!(0u64);
        gbt["potdroughtlimit"] = serde_json::json!(5u64);
        // claim_jackpots=false, claim_pot=true on an all-draw height.
        let (t0, tx0, b0, p0, c0, _, _, _, _) =
            build_template(&gbt, false, true, 100, 1, &SeedClaim::default(), false).expect("pot template");
        assert_eq!((b0, p0), (0, 2_500_000));
        assert_eq!(c0, 0x01_22a4c3);
        let l = crate::tx::parse_coinbase_layout(&tx0[0]).expect("layout");
        assert_eq!(l.vout_count, 2);
        assert_eq!(l.last_value, 2_500_000);
        let root = merkle_root(&[txid(&tx0[0])]).expect("root");
        assert_eq!(&t0.header[MERKLE_RANGE], &root[..]);
        // potcredited 0: passthrough.
        gbt["potcredited"] = serde_json::json!(0);
        let (_, _, b1, p1, c1, _, _, _, _) =
            build_template(&gbt, false, true, 100, 1, &SeedClaim::default(), false).expect("zero pot");
        assert_eq!((b1, p1), (0, 0));
        assert_eq!(c1, 0x01_22a4c3);
    }

    #[test]
    fn both_claims_splice_two_outputs() {
        let mut gbt: serde_json::Value = serde_json::from_str(GBT_FIXTURE).expect("json");
        gbt["jackpotbonus"] = serde_json::json!(1_142_856u64);
        gbt["potcredited"] = serde_json::json!(2_500_000u64);
        gbt["potdrought"] = serde_json::json!(0u64);
        gbt["potdroughtlimit"] = serde_json::json!(5u64);
        let (t, tx, b, p, c, _, _, _, _) = build_template(&gbt, true, true, 1, 1, &SeedClaim::default(), false).expect("both");
        assert_eq!((b, p), (1_142_856, 2_500_000));
        assert_eq!(c, 0x01_22a4c3);
        let l = crate::tx::parse_coinbase_layout(&tx[0]).expect("layout");
        assert_eq!(l.vout_count, 3);
        assert_eq!(l.last_value, 2_500_000);
        let root = merkle_root(&[txid(&tx[0])]).expect("root");
        assert_eq!(&t.header[MERKLE_RANGE], &root[..]);
    }

    #[test]
    fn claim_splice_merkle_covers_mempool_txs() {
        // Regression (live 2026-10-12, `hashMerkleRoot mismatch` once payout
        // txs entered the mempool): the claim splice rebuilt the header root
        // over the coinbase txid alone, dropping every mempool txid — a
        // block whose own tx list could never match its header. The root
        // must cover the whole spliced tx set.
        let mut gbt: serde_json::Value = serde_json::from_str(GBT_FIXTURE).expect("json");
        gbt["transactions"] = serde_json::json!([
            { "data": TX1_HEX, "hash": "f70cb6b3f8fb57ab4e21c7475db89f601e9d649cdaa312d5ff6bc77505edc388" },
            { "data": TX2_HEX, "hash": "e1c6db6a573fdf2008af9f0d7d0a274b67e1aae5788b2af673e1da25c40ce3c7" }
        ]);
        gbt["jackpotbonus"] = serde_json::json!(1_142_856u64);
        let (t, txs, b, p, _, _, _, _, _) =
            build_template(&gbt, true, false, 1, 1000, &SeedClaim::default(), false).expect("claim + mempool");
        assert_eq!((b, p), (1_142_856, 0));
        assert_eq!(txs.len(), 3);
        let ids: Vec<_> = txs.iter().map(|x| txid(x)).collect();
        let full = merkle_root(&ids).expect("full tree");
        let cb_only = merkle_root(&[ids[0]]).expect("cb only");
        assert_ne!(full, cb_only);
        assert_eq!(&t.header[MERKLE_RANGE], &full[..]);
        // and the assembled block agrees internally: 3 txs, root over those
        // exact bytes.
        let block = assemble_block(&t.header, 5, &txs);
        assert_eq!(block[141] as usize, 3);
        assert_eq!(&block[36..68], &full[..]);
    }

    #[test]
    fn seed_claims_splice_pinned_outputs_verbatim() {
        // Faucet/dev claims splice exact pinned-script outputs (node is
        // source of truth for both amounts and scripts); the pool never
        // invents a destination.
        let mut gbt: serde_json::Value = serde_json::from_str(GBT_FIXTURE).expect("json");
        gbt["faucetdrip"] = serde_json::json!(1_000u64);
        gbt["faucetscript"] = serde_json::json!("76a914ba38cfe49e436bb2a43898083a98becc148c204488ac");
        gbt["devdrip"] = serde_json::json!(500u64);
        gbt["devscript"] = serde_json::json!("a914f581da35b024c755572850ad3a9d6f5bd8b1afcc87");
        gbt["devepoch"] = serde_json::json!(true);
        let seed = SeedClaim {
            faucet: 1_000,
            faucet_script: hex_bytes("76a914ba38cfe49e436bb2a43898083a98becc148c204488ac").expect("hex"),
            dev: 500,
            dev_script: hex_bytes("a914f581da35b024c755572850ad3a9d6f5bd8b1afcc87").expect("hex"),
        };
        let (t, tx, b, p, c, _, f, d, _) =
            build_template(&gbt, false, false, 100, 1000, &seed, false).expect("seed template");
        // No jackpot/pot claims requested; normal value untouched.
        assert_eq!((b, p), (0, 0));
        assert_eq!((f, d), (1_000, 500));
        assert_eq!(c, 0x01_22a4c3);
        let l = crate::tx::parse_coinbase_layout(&tx[0]).expect("layout");
        assert_eq!(l.vout_count, 3); // normal + faucet + dev
        assert_eq!(l.last_value, 500); // dev splice lands last
        let root = merkle_root(&[txid(&tx[0])]).expect("root");
        assert_eq!(&t.header[MERKLE_RANGE], &root[..]);
    }

    #[test]
    fn seed_claims_require_pinned_scripts() {
        // A nonzero amount with an empty (unpinned) script claims nothing —
        // a node that disagrees with the pool about the pins degrades to
        // passthrough instead of invalid blocks.
        let mut gbt: serde_json::Value = serde_json::from_str(GBT_FIXTURE).expect("json");
        gbt["faucetdrip"] = serde_json::json!(1_000u64);
        let seed = SeedClaim {
            faucet: 1_000,
            faucet_script: Vec::new(),
            dev: 0,
            dev_script: Vec::new(),
        };
        let (_, tx, _, _, _, _, f, d, _) =
            build_template(&gbt, false, false, 100, 1000, &seed, false).expect("passthrough");
        assert_eq!((f, d), (0, 0));
        let l = crate::tx::parse_coinbase_layout(&tx[0]).expect("layout");
        assert_eq!(l.vout_count, 1);
    }

    #[test]
    fn faucet_topup_math_only_claims_the_shortfall() {
        assert_eq!(faucet_topup_amount(1_000, 0, 100_000), 1_000);
        assert_eq!(faucet_topup_amount(1_000, 99_500, 100_000), 500);
        assert_eq!(faucet_topup_amount(0, 0, 100_000), 0);
        assert_eq!(faucet_topup_amount(1_000, 100_000, 100_000), 0);
        assert_eq!(faucet_topup_amount(1_000, 200_000, 100_000), 0);
    }

    #[test]
    fn unmatured_claims_count_until_maturity_then_drop() {
        use std::collections::BTreeMap;
        let mut m = BTreeMap::new();
        m.insert(1u64, 1_000u64);
        m.insert(50u64, 500u64);
        // Both unmatured at height 60 (maturity 100).
        assert_eq!(unmatured_total(&m, 60, 100), 1_500);
        // The height-1 claim matures at 101.
        assert_eq!(unmatured_total(&m, 101, 100), 500);
        assert_eq!(unmatured_total(&m, 150, 100), 0);
        assert!(unmatured_total(&BTreeMap::new(), 60, 100) == 0);
    }

    #[test]
    fn parse_seed_fields_tolerates_absent_pins() {
        let gbt: serde_json::Value = serde_json::from_str(GBT_FIXTURE).expect("json");
        // Old node (fields absent): no seed claims.
        assert_eq!(parse_seed_fields(&gbt), (0, Vec::new(), 0, Vec::new(), false));
        let mut gbt2 = gbt.clone();
        gbt2["faucetdrip"] = serde_json::json!(1_000u64);
        gbt2["faucetscript"] = serde_json::json!("76a91400");
        gbt2["devdrip"] = serde_json::json!(50u64);
        gbt2["devscript"] = serde_json::json!("a91411");
        gbt2["devepoch"] = serde_json::json!(true);
        let (fd, fs, dd, ds, epoch) = parse_seed_fields(&gbt2);
        assert_eq!((fd, dd, epoch), (1_000, 50, true));
        assert_eq!(fs, hex_bytes("76a91400").expect("hex"));
        assert_eq!(ds, hex_bytes("a91411").expect("hex"));
    }

    #[test]
    fn decide_seed_claim_applies_topup_and_cadence() {
        // The first template and every refresh share this policy: faucet
        // claims only the shortfall to the top-up target (restored
        // in-flight claims included), dev claims the full drip on dev
        // epochs. Fail closed on unknown balance (claims nothing).
        let mut gbt: serde_json::Value = serde_json::from_str(GBT_FIXTURE).expect("json");
        gbt["height"] = serde_json::json!(200u64);
        gbt["faucetaddress"] = serde_json::json!("mAddr");
        gbt["faucetdrip"] = serde_json::json!(1_000u64);
        gbt["faucetscript"] = serde_json::json!("76a91400");
        gbt["devdrip"] = serde_json::json!(50u64);
        gbt["devscript"] = serde_json::json!("a91411");
        gbt["devepoch"] = serde_json::json!(true);

        // Empty wallet: full drip (capped), dev on cadence.
        let mut unm = std::collections::BTreeMap::new();
        let s = decide_seed_claim(&|_| Some(0), &gbt, 200, true, true, &mut unm);
        assert_eq!((s.faucet, s.dev), (1_000, 50));
        assert_eq!(s.faucet_script, hex_bytes("76a91400").expect("hex"));

        // Near-target wallet + pending in-flight claim: top-up shrinks.
        let mut unm = std::collections::BTreeMap::from([(150u64, 400u64)]);
        let s = decide_seed_claim(&|_| Some(FAUCET_TOPUP_TARGET - 500), &gbt, 200, true, true, &mut unm);
        assert_eq!(s.faucet, 100); // 500 shortfall - 400 pending = 100
        assert!(unm.contains_key(&150)); // retained: not yet matured

        // Matured pending entries prune instead of suppressing.
        let mut unm = std::collections::BTreeMap::from([(50u64, 400u64)]);
        let s = decide_seed_claim(&|_| Some(FAUCET_TOPUP_TARGET - 500), &gbt, 200, true, true, &mut unm);
        assert_eq!(s.faucet, 500);
        assert!(!unm.contains_key(&50));

        // Unknown balance fails closed (claims nothing).
        let mut unm = std::collections::BTreeMap::new();
        let s = decide_seed_claim(&|_: &str| None, &gbt, 200, true, true, &mut unm);
        assert_eq!((s.faucet, s.dev), (0, 50)); // dev is cadence-only, unaffected

        // Flags off: nothing claimed even when advertised.
        let mut unm = std::collections::BTreeMap::new();
        let s = decide_seed_claim(&|_| Some(0), &gbt, 200, false, false, &mut unm);
        assert_eq!((s.faucet, s.dev), (0, 0));
        // Not a dev-epoch height: no dev claim.
        let mut gbt2 = gbt.clone();
        gbt2["devepoch"] = serde_json::json!(false);
        let mut unm = std::collections::BTreeMap::new();
        let s = decide_seed_claim(&|_| Some(0), &gbt2, 200, true, true, &mut unm);
        assert_eq!((s.faucet, s.dev), (1_000, 0));
    }

    #[test]
    fn prize_inflow_parses_and_defaults_to_zero() {        let gbt: serde_json::Value = serde_json::from_str(GBT_FIXTURE).expect("fixture json");
        // Old node (field absent): no prize credit.
        assert_eq!(parse_prize_inflow(&gbt), 0);
        // New node: the exact per-height overflow rides through.
        let mut gbt2 = gbt.clone();
        gbt2["prizeoverflow"] = serde_json::json!(50794u64);
        assert_eq!(parse_prize_inflow(&gbt2), 50794);
        // ... and into the template, where the refresh loop records it
        // per height for the tip-change reconcile.
        let (tpl, ..) = build_template(&gbt2, true, true, 100, 1000, &SeedClaim::default(), false).expect("tpl");
        assert_eq!(tpl.prize, 50794);
    }

    #[test]
    fn build_template_claims_only_pre_committed_heights() {
        // Default divisors: the fixture's height is (almost surely) not a
        // draw height, so nonzero advisories are NOT claimed (the class
        // gate protects against advisory drift).
        let mut gbt: serde_json::Value = serde_json::from_str(GBT_FIXTURE).expect("json");
        gbt["jackpotbonus"] = serde_json::json!(500u64);
        gbt["potcredited"] = serde_json::json!(250u64);
        gbt["potdrought"] = serde_json::json!(0u64);
        gbt["potdroughtlimit"] = serde_json::json!(5u64);
        let (_, _, b0, p0, _, pity0, _, _, _) =
            build_template(&gbt, true, true, 100, 1000, &SeedClaim::default(), false).expect("plain height");
        assert_eq!((b0, p0, pity0), (0, 0, false));
        // Regtest divisor 1: every height draws — exact advertised amounts
        // are spliced verbatim.
        let (_, tx, b1, p1, _, pity1, _, _, _) =
            build_template(&gbt, true, true, 1, 1, &SeedClaim::default(), false).expect("all-draw height");
        assert_eq!((b1, p1, pity1), (500, 250, false));
        let l = crate::tx::parse_coinbase_layout(&tx[0]).expect("layout");
        assert_eq!(l.last_value, 250); // pot splice on top of bonus
                                       // Claims off: no splices even on a draw height.
        let (_, _, b2, p2, _, _, _, _, _) =
            build_template(&gbt, false, false, 1, 1, &SeedClaim::default(), false).expect("claims off");
        assert_eq!((b2, p2), (0, 0));
    }

    #[test]
    fn build_template_pity_floors_via_advertised_splash() {
        let mut gbt: serde_json::Value = serde_json::from_str(GBT_FIXTURE).expect("json");
        // Exact splash advertised (pity floor), drought at the limit, and
        // NOT a digest event (default divisors).
        gbt["potcredited"] = serde_json::json!(250_000u64);
        gbt["potdrought"] = serde_json::json!(5u64);
        gbt["potdroughtlimit"] = serde_json::json!(5u64);
        let (_, _, b0, p0, _, pity0, _, _, _) =
            build_template(&gbt, false, true, 100, 1000, &SeedClaim::default(), false).expect("pity due");
        assert_eq!((b0, p0, pity0), (0, 250_000, true));
        // Below the limit with no event: nothing claimed.
        gbt["potdrought"] = serde_json::json!(4u64);
        let (_, _, b1, p1, _, pity1, _, _, _) =
            build_template(&gbt, false, true, 100, 1000, &SeedClaim::default(), false).expect("below limit");
        assert_eq!((b1, p1, pity1), (0, 0, false));
    }

    #[test]
    fn parse_prize_draw_reads_node_advisories() {
        let gbt: serde_json::Value = serde_json::from_str(GBT_FIXTURE).expect("json");
        // Old node (no fields): no claim.
        assert_eq!(
            parse_prize_draw(&gbt),
            (0, false, 0, String::new(), Vec::new())
        );
        let mut with_draw = gbt.clone();
        with_draw["prizedrawepoch"] = serde_json::json!(14u64);
        with_draw["prizedrawvalid"] = serde_json::json!(true);
        with_draw["prizedrawremaining"] = serde_json::json!(159_200_000u64);
        with_draw["prizedrawwinner"] = serde_json::json!("LtPrizeWinner111111111111111111");
        with_draw["prizedrawscript"] =
            serde_json::json!("76a914ba38cfe49e436bb2a43898083a98becc148c204488ac");
        let (epoch, valid, remaining, winner, script) = parse_prize_draw(&with_draw);
        assert_eq!((epoch, valid, remaining), (14, true, 159_200_000));
        assert_eq!(winner, "LtPrizeWinner111111111111111111");
        assert_eq!(
            script,
            hex_bytes("76a914ba38cfe49e436bb2a43898083a98becc148c204488ac").expect("hex")
        );
        // Invalid draw with a nonzero budget: no claim (fail closed).
        let mut invalid = with_draw.clone();
        invalid["prizedrawvalid"] = serde_json::json!(false);
        let (_, valid2, _, _, _) = parse_prize_draw(&invalid);
        assert!(!valid2);
        // Zero remaining: no claim even when valid.
        let mut zero = with_draw.clone();
        zero["prizedrawremaining"] = serde_json::json!(0u64);
        let (_, valid3, remaining3, _, _) = parse_prize_draw(&zero);
        assert!(valid3);
        assert_eq!(remaining3, 0);
        // Bad script hex: no claim (treated as missing).
        let mut badhex = with_draw;
        badhex["prizedrawscript"] = serde_json::json!("zz");
        let (_, _, _, _, script4) = parse_prize_draw(&badhex);
        assert!(script4.is_empty());
    }

    const PRIZE_WINNER_SCRIPT: &str =
        "76a914ba38cfe49e436bb2a43898083a98becc148c204488ac";

    fn gbt_with_prize_draw() -> serde_json::Value {
        let mut gbt: serde_json::Value = serde_json::from_str(GBT_FIXTURE).expect("json");
        gbt["prizedrawepoch"] = serde_json::json!(14u64);
        gbt["prizedrawvalid"] = serde_json::json!(true);
        gbt["prizedrawremaining"] = serde_json::json!(159_200_000u64);
        gbt["prizedrawwinner"] = serde_json::json!("LtPrizeWinner111111111111111111");
        gbt["prizedrawscript"] = serde_json::json!(PRIZE_WINNER_SCRIPT);
        gbt
    }

    #[test]
    fn prize_claim_splices_verbatim_to_winner_script() {
        // The pool claims nothing on its own: the node names the winner
        // and the exact remaining budget, and the pool splices it
        // verbatim to that script — a third-party output, never miner
        // receipts.
        let gbt = gbt_with_prize_draw();
        let (t, txs, b, p, c, _, f, d, prize) =
            build_template(&gbt, false, false, 100, 1000, &SeedClaim::default(), true)
                .expect("prize template");
        assert_eq!((b, p, f, d), (0, 0, 0, 0)); // only the prize leg fires
        assert_eq!((prize.amount, prize.winner.as_str()), (159_200_000, "LtPrizeWinner111111111111111111"));
        assert_eq!(c, 0x01_22a4c3); // normal value untouched
        let l = crate::tx::parse_coinbase_layout(&txs[0]).expect("layout");
        assert_eq!(l.vout_count, 2); // normal + dedicated claim output
        assert_eq!(l.last_value, 159_200_000);
        assert_eq!(
            l.last_script,
            hex_bytes(PRIZE_WINNER_SCRIPT).expect("hex")
        );
        // The normal output keeps the node's own script and value.
        let scripts = crate::tx::output_scripts(&txs[0]).expect("scripts");
        assert_eq!(scripts.len(), 2);
        let fixture_script =
            hex_bytes("2103f9c81821efd89ad66e9fc56e0a3d870aa8ce9a424396cb5c856fb6a609977666ac")
                .expect("hex");
        assert_eq!(scripts[0], fixture_script);
        assert_eq!(scripts[1], hex_bytes(PRIZE_WINNER_SCRIPT).expect("hex"));
        let root = merkle_root(&[txid(&txs[0])]).expect("root");
        assert_eq!(&t.header[MERKLE_RANGE], &root[..]);
    }

    #[test]
    fn prize_claim_skips_when_winner_script_already_paid() {
        // Directed-sum rule (doc 06 §6.7.3): when the winner's script
        // already pays in the template coinbase (here: the pool's own
        // payout address won the draw), appending the claim would make
        // the sum drift and the node would refuse the block. The pool
        // skips the claim instead of losing the find.
        let mut gbt = gbt_with_prize_draw();
        gbt["prizedrawscript"] = serde_json::json!(
            "2103f9c81821efd89ad66e9fc56e0a3d870aa8ce9a424396cb5c856fb6a609977666ac"
        );
        let (_, txs, _, _, _, _, _, _, prize) =
            build_template(&gbt, false, false, 100, 1000, &SeedClaim::default(), true)
                .expect("collision skips");
        assert_eq!(prize.amount, 0);
        let l = crate::tx::parse_coinbase_layout(&txs[0]).expect("layout");
        assert_eq!(l.vout_count, 1); // untouched normal template
    }

    #[test]
    fn prize_claim_off_flag_leaves_template_untouched() {
        let gbt = gbt_with_prize_draw();
        let (_, txs, _, _, c, _, _, _, prize) =
            build_template(&gbt, false, false, 100, 1000, &SeedClaim::default(), false)
                .expect("flag off");
        assert_eq!(prize.amount, 0);
        assert_eq!(c, 0x01_22a4c3);
        let l = crate::tx::parse_coinbase_layout(&txs[0]).expect("layout");
        assert_eq!(l.vout_count, 1);
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
