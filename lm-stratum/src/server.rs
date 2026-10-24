//! Threaded reference pool server: miners in, ledger + money out.
//!
//! One OS thread per connection plus one accept thread; all shared state
//! behind short-lived mutexes, job broadcasts polled (no async runtime by
//! design — this must build and run on a miniPC with nothing but rustc).
//!
//! Per-submit flow: job lookup → dupe check → real LuckyHash verification
//! against the pool target → ledger share (work = expected max-target
//! hashes) → on network-target hit, authoritative chain submission via the
//! [`TemplateProvider`](crate::provider::TemplateProvider): reserve update,
//! prize-fund credit, jackpot split over the PPLNS window, block record.
//! A share the chain rejects (height raced) is still an accepted share —
//! the pool's check is advisory, the chain's authoritative, and only the
//! chain mints money.

use crate::provider::{JobTemplate, TemplateProvider};
use crate::v1::{
    error_response, notify, response, work_for_share, Dedupe, Job, JobRegistry, Message, Verdict,
};
use crate::v2::{
    from_frame, to_frame, CloseChannel, LuckyJob, LuckySubmit, OpenMiningChannelError,
    OpenStandardMiningChannel, OpenStandardMiningChannelSuccess, SetTarget, SetupConnectionSuccess,
    SubmitSharesError, SubmitSharesSuccess, MSG_CLOSE_CHANNEL, MSG_LUCKY_SUBMIT,
    MSG_OPEN_STANDARD_MINING_CHANNEL, PROTOCOL_MINING,
};
use crate::v2_session::{accept_setup_connection, SessionError, V2Session};
use crate::vardiff::{target_for_work, Vardiff};
use crate::workerstats::{WorkerSnapshot, WorkerStats};
use lm_pool::ledger::{Ledger, MinerAgg, ShareRec, PPLNS_WINDOW_SHARES as LEDGER_PPLNS};
use lm_pool::payout::PayoutLedger;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex,
};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Background template refresh cadence. Matches the connection read-tick
/// granularity so new jobs reach miners as promptly as before; the
/// provider's own due-check (NodeProvider::REFRESH_INTERVAL) still gates
/// actual node polls.
const REFRESH_TICK_MS: u64 = 100;

/// One found block, as the pool paid it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BlockRecord {
    pub height: u64,
    pub finder: String,
    /// Net block value the pool received (normal coinbase, no claims).
    pub value: u64,
    pub bonus: u64,
    /// Pot splash the coinbase claimed (0 without a pot event).
    pub pot: u64,
    /// `default` + `alias`: pre-rename state files named this field
    /// `league_overflow`; both spellings load (0 when absent).
    #[serde(default, alias = "league_overflow")]
    pub prize_inflow: u64,
    /// Seed-fund outputs in the coinbase (third-party, never miner-owed).
    /// `default` so state files written before this field still load.
    #[serde(default)]
    pub faucet: u64,
    #[serde(default)]
    pub dev: u64,
    /// Consensus Luckynode Draw claim landed in this block (0 = none):
    /// a third-party output to the epoch winner, never miner-owed
    /// (doc 06 §6.7). `default` for old state files.
    #[serde(default)]
    pub prize_claim: u64,
    /// Winner address the claim above paid ("" when no claim).
    #[serde(default)]
    pub prize_winner: String,
    /// PPLNS split of every claimed amount (jackpot + pot), per miner.
    pub payouts: Vec<(String, u64)>,
}
/// Retained block history (finding G spirit: the dashboard must never
/// OOM the pool; the cap is deep enough for every UI window).
pub const BLOCK_HISTORY_CAP: usize = 1000;

/// Retained per-height prize-credit entries (the explorer polls the
/// recent window; older entries age out of the feed but the prize
/// balance itself is unaffected).
pub const PRIZE_CREDIT_LOG_CAP: usize = 256;

/// Pool counters.
#[derive(Debug, Clone, Default)]
pub struct Stats {
    pub shares: u64,
    pub rejects: u64,
    pub blocks: u64,
    /// Network-meeting finds the chain REFUSED (submitblock rejected).
    /// Zero is the only healthy value; every increment is a lost block
    /// and must be investigated, not just counted.
    pub block_rejects: u64,
}

struct JobState {
    job: Job,
    version: u64,
}

/// RAII connection accounting: releases the accept-loop caps when the
/// connection thread exits (any path, including panics).
struct ConnGuard<P: TemplateProvider + 'static> {
    shared: Arc<Shared<P>>,
    ip: std::net::IpAddr,
}

impl<P: TemplateProvider + 'static> Drop for ConnGuard<P> {
    fn drop(&mut self) {
        // Poison-tolerant by policy: connection accounting is plain data,
        // and a panic while holding this lock must not cascade (a panicking
        // Drop during unwinding aborts the process).
        use std::sync::atomic::Ordering as AtomicOrdering;
        self.shared.conn_total.fetch_sub(1, AtomicOrdering::SeqCst);
        let mut m = self
            .shared
            .conn_per_ip
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(n) = m.get_mut(&self.ip) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                m.remove(&self.ip);
            }
        }
    }
}

/// Connection bounds: the accept loop used to
/// spawn an unbounded thread per TCP connection and the read loop
/// accumulated an uncapped line — slowloris OOM on a public bind.
const MAX_CONNS: usize = 2048;
const MAX_CONNS_PER_IP: usize = 64;
/// Legitimate stratum messages are well under 2 KiB; 64 KiB is generous.
const MAX_LINE_BYTES: usize = 64 * 1024;
/// Worker names double as payout addresses (≤ 35 chars) and persist into
/// WorkerStats, per-miner payout settings lookups, and the state file.
/// Cap them far below the line cap: an unbounded name is RAM/state bloat
/// on an open pool (payout safety is unaffected — non-address names just
/// hold). V2's codec already bounds user identities to 255 bytes; this
/// is the tighter uniform ceiling.
pub const MAX_WORKER_NAME_LEN: usize = 128;
/// A client must authorize within this many seconds of connecting.
const AUTH_DEADLINE_SECS: u64 = 30;
/// Any connection silent this long (no complete line) is dropped.
const IDLE_DEADLINE_SECS: u64 = 120;
/// Open V2 standard channels per Noise session. A proxy may legitimately
/// serve several workers down one session; nobody needs thousands, and an
/// uncapped open-flood grew memory per channel forever.
const MAX_CHANNELS_PER_SESSION: usize = 64;

/// Current-epoch Luckynode Draw status for the dashboard (display
/// only — claims are spliced from the same advisories at template
/// build). Epoch 0 = no advisory (mock provider or pre-draw node).
#[derive(Debug, Clone, Default)]
pub struct DrawStatus {
    pub epoch: u64,
    pub budget: u64,
    pub remaining: u64,
    pub winner: String,
}

struct Shared<P> {
    provider: Mutex<P>,
    ledger: Mutex<Ledger>,
    blocks: Mutex<VecDeque<BlockRecord>>,
    stats: Mutex<Stats>,
    broadcast: Mutex<JobState>,
    /// Pool-wide work-identity dedupe: one share can be replayed on a
    /// fresh connection (empty per-connection job window) and each
    /// connection's registry must reject it, so the memory lives here and
    /// is locked around each evaluate — not inside the registries.
    dedupe: Mutex<Dedupe>,
    /// Prospective prize inflow per template height (from the node's
    /// own `prizeoverflow` advisory): credited to the prize ledger when
    /// the height's block lands — whether the pool mined it (accept path)
    /// or an external finder did (tip-change reconcile in the refresh
    /// loop). Without this, staker/peer-found blocks' overflow never
    /// entered any pool ledger and Luckynode Draw budgets leaked on a
    /// 50/50 chain.
    tip_overflow: Mutex<BTreeMap<u64, u64>>,
    /// Heights whose prize inflow is already credited (either path).
    /// Credit-once gate: the accept thread and the refresh thread race on
    /// our own finds, and exactly one of them may credit a height.
    prize_credited: Mutex<HashSet<u64>>,
    /// Per-height prize inflow actually credited (height → sats).
    /// Persisted with the state file: staker/peer-found blocks never
    /// appear in the pool's block history, so this log is the only
    /// durable record of their game-fund inflow (the explorer mirrors
    /// it into its per-block game view). Bounded to the recent window.
    prize_credit_log: Mutex<BTreeMap<u64, u64>>,
    /// Highest tip height reconciled for external prize credit (0 =
    /// uninitialized; first tick starts from the live tip, no backfill —
    /// same restart rule as the draw loop's epoch init).
    prize_tip: Mutex<u64>,
    /// Latest consensus draw advisory seen on a template (refreshed
    /// every provider tick — the dashboard's current-epoch card).
    latest_draw: Mutex<DrawStatus>,
    /// Live connection accounting for the accept caps (RAII-guarded).
    conn_total: std::sync::atomic::AtomicUsize,
    conn_per_ip: Mutex<std::collections::HashMap<std::net::IpAddr, usize>>,
    /// Per-worker dashboard stats (accepted/rejected, rolling hashrate,
    /// liveness) —.
    workers: Mutex<WorkerStats>,
    /// Per-miner value accounting: what each miner
    /// earned from blocks (pro-rata) and splits, minus what paid out.
    balances: Mutex<PayoutLedger>,
    /// Lifetime miner-owed coinbase accepted by this pool: the normal
    /// block value plus every jackpot bonus and pot splash, summed over
    /// all accepted blocks. Prize inflow and the seed-fund faucet/dev
    /// outputs are third-party flows with their own ledgers and are NOT
    /// included — this counter must equal owed + paid + fees at all
    /// times (the /api/stats identity), and any drift is a bug.
    /// Persisted in the state file like the ledger itself.
    credited_total: Mutex<u64>,
    /// Per-miner payout settings (address/threshold overrides,.
    payout_settings: Mutex<PayoutSettings>,
    /// Per-worker device-attestation verdicts (doc 03 §3.6) — verdicts
    /// only, no identity. Persisted with the state file.
    attestations: Mutex<Attestations>,
    /// Pool fee in per-myriad, applied when blocks credit balances.
    pool_fee: Mutex<u16>,
    /// Dev-fee slice (miner id, per-myriad of the block), taken from the
    /// operator fee pot via `allocate_dev` — docs/04 §4.2. `None` = off.
    dev_fee: Mutex<Option<(String, u16)>>,
    pool_password: Option<String>,
    /// Per-worker tokens (worker -> token). When set, it is authoritative:
    /// only listed workers with a matching token may authorize. Otherwise
    /// `pool_password` applies (open when unset).
    worker_tokens: Option<HashMap<String, String>>,
    /// TLS server config; `None` = plaintext wire (default).
    tls: Option<Arc<rustls::ServerConfig>>,
    job_counter: AtomicU64,
    shutdown: AtomicBool,
}

/// Per-miner payout settings: an optional payout address
/// override (the default is the worker name, which doubles as an address)
/// and an optional payout threshold override (the default is the pool's
/// global threshold).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MinerSettings {
    /// Payout address override (validated before storing).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    /// Payout threshold override, base units.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threshold: Option<u64>,
}

/// Per-miner payout settings keyed by miner id (persisted with the pool
/// state file).
pub type PayoutSettings = std::collections::BTreeMap<String, MinerSettings>;

/// One device-attestation verdict (doc 03 §3.6, doc 07 §7.4):
/// verdict-only, no identity — the worker name is the key, and `provider`
/// names the verifier that produced the verdict. `"self"` is the
/// unverified testnet stub used until a Play Integrity verifier is
/// configured on the pool.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Attestation {
    /// Verifier that produced the verdict.
    pub provider: String,
    /// When the pool recorded it (unix seconds).
    pub time: u64,
    /// Free-form verdict detail (device model, payload digest).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
}

/// Per-worker attestations keyed by miner id (persisted with the pool state).
pub type Attestations = std::collections::BTreeMap<String, Attestation>;

/// How long an attestation stays fresh. Phone-division entry is per season;
/// an older verdict stops counting.
pub const ATTESTATION_TTL_SECS: u64 = 30 * 24 * 60 * 60;

/// One miner's dashboard row + value totals.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MinerRow {
    pub miner: String,
    pub name: String,
    pub agent: String,
    pub shares: u64,
    pub rejects: u64,
    pub online: bool,
    pub last_seen: u64,
    /// Rolling hashrate over 60s/10m/1h/24h (H/s).
    pub rates: [f64; 4],
    /// Value owed but not yet paid (base units).
    pub owed: u64,
    /// Lifetime paid (base units).
    pub paid: u64,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Parse a workers file: one `worker:token` per line; `#` comments and
/// blank lines ignored. Worker names are matched exactly (they double as
/// payout addresses); the token is everything after the first colon.
pub fn parse_workers_file(content: &str) -> Result<HashMap<String, String>, String> {
    let mut out = HashMap::new();
    for (n, raw) in content.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (worker, token) = line
            .split_once(':')
            .ok_or_else(|| format!("line {}: expected worker:token", n + 1))?;
        let (worker, token) = (worker.trim(), token.trim());
        if worker.is_empty() || token.is_empty() {
            return Err(format!("line {}: empty worker or token", n + 1));
        }
        if out.insert(worker.to_string(), token.to_string()).is_some() {
            return Err(format!("line {}: duplicate worker '{worker}'", n + 1));
        }
    }
    Ok(out)
}

/// Branch-free byte comparison for secrets (length equality still
/// short-circuits — the usual accepted leak; the byte walk itself gives
/// no per-character timing oracle).
pub fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Authorization decision. A per-worker token map (when configured) is
/// authoritative: the worker must exist and the token must match. Otherwise
/// the shared pool password applies (open when unset).
fn auth_ok(
    tokens: Option<&HashMap<String, String>>,
    password: Option<&str>,
    worker: &str,
    pass: &str,
) -> bool {
    if let Some(tokens) = tokens {
        return tokens.get(worker).map(|t| ct_eq(t, pass)).unwrap_or(false);
    }
    match password {
        Some(pw) => ct_eq(pw, pass),
        None => true,
    }
}

/// Read the current broadcast job + version. Hot path for connection
/// threads: never touches the provider (finding D — they used to poll it
/// inline on every tick, serializing all miners behind one node RPC).
fn broadcast_job<P>(shared: &Shared<P>) -> (Job, u64) {
    let bc = shared
        .broadcast
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    (bc.job.clone(), bc.version)
}

/// Snapshot of a template's draw advisory for the dashboard card.
fn draw_status_of(tpl: &JobTemplate) -> DrawStatus {
    DrawStatus {
        epoch: tpl.draw_epoch,
        budget: tpl.draw_budget,
        remaining: tpl.draw_remaining,
        winner: tpl.draw_winner.clone(),
    }
}

/// Background refresh loop: poll the provider and publish new jobs. The
/// ONLY thread that calls `provider.current()` after startup — a slow
/// node RPC stalls this thread, not the miners. Submits still take the
/// provider lock (serialized by design: last-write-wins would
/// double-submit a block), but nothing waits on a refresh anymore.
fn refresh_loop<P: TemplateProvider + 'static>(shared: Arc<Shared<P>>) {
    while !shared.shutdown.load(Ordering::Relaxed) {
        let tpl = shared
            .provider
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .current();
        // Current-epoch draw card: refresh every tick (not just on new
        // jobs) so budget/remaining track the live template.
        *shared
            .latest_draw
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = draw_status_of(&tpl);
        {
            // The broadcast guard must drop BEFORE the tick sleep: holding
            // it across the sleep starves every connection thread (release
            // caught it — a fast provider re-locks within nanoseconds, so
            // a waiting handler can lose the race for minutes; the e2e
            // suite timed out at 10 s per RPC).
            let mut bc = shared
                .broadcast
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // A new job exists only when the TEMPLATE BYTES change (height
            // or header). Timer-based rotation duplicated jobs whenever the
            // node's template cadence was slower than the timer: two
            // "different" jobs shared one header, so the miner re-found the
            // same nonce for each and the pool counted duplicate work as
            // separate shares (found in testing: nonce 20796 on jobs 1f/20/21).
            if tpl.height != bc.job.height || tpl.header != bc.job.header {
                let id = shared.job_counter.fetch_add(1, Ordering::SeqCst);
                bc.job = Job {
                    id: format!("{id:08x}"),
                    header: tpl.header,
                    height: tpl.height,
                    pool_target: tpl.pool_target,
                    net_target: tpl.net_target,
                };
                bc.version += 1;
                // Prospective prize inflow for this height (the node's
                // own advisory): the accept path and the tip reconcile
                // below share one credit-once gate, so recording here is
                // safe even when our own find lands first.
                {
                    let mut pend = shared
                        .tip_overflow
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    pend.insert(tpl.height, tpl.prize);
                    while pend.len() > 64 {
                        pend.pop_first();
                    }
                }
            }
        }
        // Tip-change reconcile: credit prize inflow for heights the pool
        // did NOT mine (staker/peer finds). The gate makes this race-free
        // against the accept path on our own blocks; heights with no
        // prospective record (pool was down/behind) are skipped and logged
        // — never guessed.
        {
            let tip = tpl.height.saturating_sub(1);
            let mut synced = shared
                .prize_tip
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if *synced == 0 {
                *synced = tip;
            } else if tip > *synced {
                let old = *synced;
                let pend = shared
                    .tip_overflow
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let done = shared
                    .prize_credited
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let missing = missing_prize_credits(old, tip, &pend, &done);
                // Heights neither credited nor known-prospective: the pool
                // was down/behind for them — logged, never guessed. (Heights
                // the accept path already credited are NOT unknown.)
                let unknown = (old + 1..=tip)
                    .filter(|h| !done.contains(h) && !pend.contains_key(h))
                    .count();
                drop(pend);
                drop(done);
                for (h, amount) in &missing {
                    if *amount > 0 {
                        credit_prize_once(&shared, *h, *amount, "tip");
                    } else {
                        shared
                            .prize_credited
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .insert(*h);
                    }
                }
                if unknown > 0 {
                    eprintln!(
                        "pool: tip {old}->{tip}: {unknown} heights had no prospective prize record (skipped)"
                    );
                }
                let mut done = shared
                    .prize_credited
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                done.retain(|&h| h + 128 > tip);
                *synced = tip;
            } else if tip < *synced {
                // Reorg: drop prospective records above the new tip.
                // Already-credited heights stand (same no-rollback rule
                // as miner balances).
                let old = *synced;
                shared
                    .tip_overflow
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .retain(|&h, _| h <= tip + 1);
                *synced = tip;
                eprintln!(
                    "pool: reorg? tip {tip} below synced {old} — prospective prize map reset"
                );
            }
        }
        std::thread::sleep(Duration::from_millis(REFRESH_TICK_MS));
    }
}

/// Heights in (old_tip, new_tip] whose prize inflow still needs
/// crediting: known prospectively and not yet credited by either path.
/// Pure (unit-tested); the caller holds no locks while computing.
fn missing_prize_credits(
    old_tip: u64,
    new_tip: u64,
    pending: &BTreeMap<u64, u64>,
    credited: &HashSet<u64>,
) -> Vec<(u64, u64)> {
    if new_tip <= old_tip {
        return Vec::new();
    }
    (old_tip + 1..=new_tip)
        .filter_map(|h| {
            if credited.contains(&h) {
                return None;
            }
            pending.get(&h).map(|&o| (h, o))
        })
        .collect()
}

/// Record a height's prize inflow exactly once (the accept thread and
/// the refresh thread race on our own finds). Display feed only — no
/// money moves here: since the consensus-draw cutover the fund pays
/// winners directly (doc 06 §6.7), so the pool keeps no prize balance;
/// the log exists for the explorer's per-block game view. Returns true
/// when this call performed the record.
fn credit_prize_once<P>(shared: &Shared<P>, height: u64, amount: u64, source: &str) -> bool {
    {
        let mut done = shared
            .prize_credited
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !done.insert(height) {
            return false;
        }
    }
    if amount > 0 {
        let mut log = shared
            .prize_credit_log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        log.insert(height, amount);
        // Keep the persisted log bounded (the explorer only needs the
        // recent window).
        while log.len() > PRIZE_CREDIT_LOG_CAP {
            log.pop_first();
        }
    }
    eprintln!("pool: prize +{amount} @height {height} ({source})");
    true
}

/// Shared money path for an accepted block-finding share (V1 and V2 call
/// this — one code path mints money): chain submission is authoritative,
/// then the prize credit, the jackpot/pot PPLNS splits, per-miner value
/// accounting, and the block record. A share the chain rejects (height
/// raced) is still an accepted share — the pool's check is advisory, the
/// chain's authoritative, and only the chain mints money.
fn apply_block_found<P: TemplateProvider + 'static>(
    shared: &Shared<P>,
    miner_id: String,
    job: &Job,
    nonce: u64,
) {
    let outcome = shared
        .provider
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .submit_block(&job.header, job.height, nonce);
    if !outcome.accepted {
        // Lost finds must be loud: a rejected submitblock is money on
        // the floor and a template/consensus bug until proven
        // otherwise. Log with full context, count it, and surface it on
        // /api/stats (the dashboard and the monitor both read that).
        shared
            .stats
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .block_rejects += 1;
        eprintln!(
            "BLOCK SUBMIT REJECTED: height {} nonce {} digest {} — the share met the network target; check the node log",
            job.height,
            nonce,
            outcome.out.iter().map(|b| format!("{b:02x}")).collect::<String>()
        );
        return;
    }
    if outcome.accepted {
        let now = now_secs();
        // Prize credit goes through the credit-once gate: the refresh
        // thread may reconcile this same height if it sees the new tip
        // before this thread runs.
        credit_prize_once(shared, job.height, outcome.prize_inflow, "accept");
        let ledger = shared
            .ledger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Both claimed amounts split over the PPLNS window (each with the
        // reference sqrt/cap rules); the combined list is what pays out.
        let mut payouts = ledger.jackpot_payout_shares(outcome.bonus, LEDGER_PPLNS, now);
        payouts.extend(ledger.jackpot_payout_shares(outcome.pot, LEDGER_PPLNS, now));
        // Per-miner value accounting: the normal block
        // value by PPLNS weight plus the block's own split payouts.
        let weights: Vec<(String, u64)> = ledger
            .aggregate_shares(LEDGER_PPLNS, now)
            .iter()
            .map(|a| (a.miner.clone(), a.eff_work))
            .collect();
        let mut balances = shared
            .balances
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let fee = *shared
            .pool_fee
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        balances.credit_block(outcome.coinbase, &weights, fee);
        balances.credit_split(&payouts);
        if let Some((dev_id, pp)) = shared
            .dev_fee
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
        {
            let dev = outcome.coinbase as u128 * u64::from(pp) as u128
                / lm_pool::payout::FEE_DENOM_PERMYRIAD as u128;
            let moved = balances.allocate_dev(&dev_id, dev as u64);
            if moved > 0 {
                eprintln!(
                    "dev fee: {} -> miner {} ({} blocks paid)",
                    moved,
                    dev_id,
                    shared
                        .blocks
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .len()
                );
            }
        }
        drop(balances);
        let mut blocks = shared
            .blocks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        blocks.push_back(BlockRecord {
            height: job.height,
            finder: miner_id,
            value: outcome.coinbase,
            bonus: outcome.bonus,
            pot: outcome.pot,
            prize_inflow: outcome.prize_inflow,
            prize_claim: outcome.prize_claim,
            prize_winner: outcome.prize_winner.clone(),
            faucet: outcome.faucet,
            dev: outcome.dev,
            payouts,
        });
        // Lifetime miner-owed coinbase: every sat credited above came
        // out of exactly these three fields (prize inflow, the draw
        // claim, and faucet/dev never touch the miner ledger), so their
        // running sum must always equal owed + paid + fees.
        {
            let mut credited = shared
                .credited_total
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *credited = credited
                .saturating_add(outcome.coinbase)
                .saturating_add(outcome.bonus)
                .saturating_add(outcome.pot);
        }
        while blocks.len() > BLOCK_HISTORY_CAP {
            blocks.pop_front();
        }
        drop(blocks);
        shared
            .stats
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .blocks += 1;
        // Seed-fund claims ride along for the record (third-party outputs —
        // never miner-owed, so no ledger impact; the node already validated
        // and debited the reserves).
        if outcome.faucet > 0 || outcome.dev > 0 {
            println!(
                "seed claim landed: height {} faucet {} dev {}",
                job.height, outcome.faucet, outcome.dev
            );
        }
        // Consensus draw claims likewise ride along for the record: the
        // winner is a third party, never miner-owed, so no ledger impact
        // and no entry in the payout identity (the node validated and
        // debited the prize fund).
        if outcome.prize_claim > 0 {
            println!(
                "prize claim landed: height {} winner {} amount {}",
                job.height, outcome.prize_winner, outcome.prize_claim
            );
        }
    }
}

fn handle_conn<P: TemplateProvider + 'static>(
    stream: TcpStream,
    shared: Arc<Shared<P>>,
    tls_override: Option<&Arc<rustls::ServerConfig>>,
) {
    let _ = stream.set_read_timeout(Some(Duration::from_millis(100)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    // Kill Nagle: `writeln!` issues body and newline as two writes, and
    // Nagle + delayed ACK stretch the gap to ~100 ms — right at the read
    // tick, which is how half-submits got lost.
    let _ = stream.set_nodelay(true);
    let io = match crate::tls::accept(stream, tls_override.or(shared.tls.as_ref())) {
        Ok(io) => io,
        Err(_) => return,
    };
    let mut stream = BufReader::new(io);
    // 16 jobs at 30s rotation = 8 minutes of job lifetime: a slow find
    // (p=1/65536 has a long tail) must still be submittable.
    let mut registry = JobRegistry::new(16);
    let mut authorized: Option<String> = None;
    // Agent string from subscribe (params[0]); labels the worker's device
    // on the dashboard.
    let mut agent = String::new();
    // Per-connection vardiff: template rotations reset the override target;
    // retargets derive conn-local jobs (same height, fresh id, clean=false).
    let mut vardiff = Vardiff::new();
    let mut _vd_target: Option<([u8; 32], u64)> = None; // (pool_target, template height)
    let mut seen_version = 0u64;
    let mut sent_height = 0u64;
    let mut line = String::new();
    let connected_at = std::time::Instant::now();
    let mut last_msg = connected_at;

    // Seed the registry so the first job is evaluable even before broadcast.
    let (first, _) = broadcast_job(&shared);
    registry.issue(first);

    loop {
        if shared.shutdown.load(Ordering::Relaxed) {
            break;
        }
        // Line cap applies to PARTIAL lines too: read_line appends bytes
        // even when it returns a timeout error, and the post-read check
        // below only runs after a COMPLETE line — a peer that streams
        // bytes without ever sending a newline grew `line` without bound
        // (audit finding: slowloris OOM on the public port). Checking at
        // the top of every tick bounds memory no matter how the bytes
        // arrive.
        if line.len() > MAX_LINE_BYTES {
            break;
        }
        // Broadcast check first: fresh jobs must reach miners promptly.
        let (cur, cur_version) = broadcast_job(&shared);
        if authorized.is_some() && cur_version != seen_version {
            seen_version = cur_version;
            _vd_target = Some((cur.pool_target, cur.height));
            registry.issue(cur.clone());
            let clean = cur.height != sent_height;
            sent_height = cur.height;
            let msg = notify(&cur, clean);
            if writeln!(stream.get_mut(), "{msg}").is_err() {
                break;
            }
        }
        // Partial reads MUST survive the tick timeout: a message split
        // across it (TCP segmentation, Nagle) was silently discarded by the
        // old clear-then-read pattern — half a submit got eaten, the miner
        // never got a response and reconnected (found in testing with pcaps:
        // submit body ACKed, pool silent 10 s, then FIN). Keep accumulating
        // in `line` until a full line arrives.
        match stream.read_line(&mut line) {
            Ok(0) => break, // peer closed
            Ok(_) => {}
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                // Deadlines: authorize promptly,
                // and no connection may sit silent forever.
                if authorized.is_none() && connected_at.elapsed().as_secs() > AUTH_DEADLINE_SECS {
                    break; // never authorized — free the thread
                }
                if last_msg.elapsed().as_secs() > IDLE_DEADLINE_SECS {
                    break;
                }
                continue;
            }
            Err(_) => break,
        }
        if line.len() > MAX_LINE_BYTES {
            // Slowloris / memory guard: no legitimate message is this big.
            break;
        }
        let msg_line = std::mem::take(&mut line);
        last_msg = std::time::Instant::now();
        let trimmed = msg_line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let msg: Message = match serde_json::from_str(trimmed) {
            Ok(m) => m,
            Err(_) => {
                let _ = writeln!(
                    stream.get_mut(),
                    "{}",
                    error_response(&serde_json::Value::Null, -32700, "parse error")
                );
                continue;
            }
        };
        let id = msg.id.clone().unwrap_or(serde_json::Value::Null);
        let method = msg.method.as_deref().unwrap_or("");
        match method {
            "mining.subscribe" => {
                if let Some(a) = msg
                    .params
                    .as_ref()
                    .and_then(|p| p.as_array())
                    .and_then(|a| a.first())
                    .and_then(|v| v.as_str())
                {
                    // Subscribe agents are arbitrary client strings and feed
                    // the dashboard verbatim — cap like worker names (the
                    // dashboard escapes them, this bounds stored state).
                    let mut a = a.to_string();
                    a.truncate(MAX_WORKER_NAME_LEN);
                    agent = a;
                }
                let _ = writeln!(
                    stream.get_mut(),
                    "{}",
                    response(&id, serde_json::json!([["mining.notify", "sid"], "", 0]))
                );
            }
            "mining.authorize" => {
                let arr = msg.params.as_ref().and_then(|p| p.as_array());
                let worker = arr
                    .and_then(|a| a.first())
                    .and_then(|w| w.as_str())
                    .unwrap_or("anonymous")
                    .to_string();
                let pass = arr
                    .and_then(|a| a.get(1))
                    .and_then(|w| w.as_str())
                    .unwrap_or("");
                if worker.len() > MAX_WORKER_NAME_LEN {
                    let _ = writeln!(
                        stream.get_mut(),
                        "{}",
                        error_response(&id, -32600, "worker name too long (max 128 chars)")
                    );
                    continue;
                }
                if !auth_ok(
                    shared.worker_tokens.as_ref(),
                    shared.pool_password.as_deref(),
                    &worker,
                    pass,
                ) {
                    let _ = writeln!(
                        stream.get_mut(),
                        "{}",
                        error_response(&id, -32600, "invalid credentials")
                    );
                    continue;
                }
                // Money identity = the worker name itself: FNV-1a worker ids were forgeable and the
                // payout ledger keyed by them could be redirected.
                shared
                    .workers
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .authorized(&worker, &agent, now_secs());
                authorized = Some(worker);
                _vd_target = Some((cur.pool_target, cur.height));
                // Immediately push the current job (clean start).
                registry.issue(cur.clone());
                seen_version = cur_version;
                sent_height = cur.height;
                let _ = writeln!(
                    stream.get_mut(),
                    "{}",
                    response(&id, serde_json::Value::Bool(true))
                );
                let msg = notify(&cur, true);
                if writeln!(stream.get_mut(), "{msg}").is_err() {
                    break;
                }
            }
            "mining.submit" => {
                let Some(miner_id) = authorized.clone() else {
                    let _ = writeln!(
                        stream.get_mut(),
                        "{}",
                        error_response(&id, -32600, "authorize first")
                    );
                    continue;
                };
                let arr = msg.params.as_ref().and_then(|p| p.as_array());
                let (job_id, nonce_hex) = match arr {
                    Some(a) if a.len() >= 3 => (
                        a[1].as_str().unwrap_or("").to_string(),
                        a[2].as_str().unwrap_or("").to_string(),
                    ),
                    _ => {
                        let _ = writeln!(
                            stream.get_mut(),
                            "{}",
                            error_response(&id, -32600, "submit needs [worker, job, nonce]")
                        );
                        continue;
                    }
                };
                // Worker field is informational; the authorized identity pays.
                // Hash runs outside the pool-wide dedupe lock (the lock
                // is taken only for the brief peek/insert inside).
                let (verdict, submit) = registry.evaluate(&job_id, &nonce_hex, &shared.dedupe);
                let (accept, err) = verdict.response();
                match (accept, submit) {
                    (true, Some(s)) => {
                        let now = now_secs();
                        let work = work_for_share(&s.job.pool_target);
                        shared
                            .ledger
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .submit(ShareRec {
                                miner: miner_id.clone(),
                                ts: now,
                                work,
                                overnight: false,
                            });
                        shared
                            .stats
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .shares += 1;
                        shared
                            .workers
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .accepted(&miner_id, work, now);
                        // Vardiff: adapt this connection's pool target, same template.
                        if let Some(new_work) = vardiff.record(now, work) {
                            let new_target = target_for_work(new_work);
                            _vd_target = Some((new_target, s.job.height));
                            let nid = shared.job_counter.fetch_add(1, Ordering::SeqCst);
                            let derived = Job {
                                id: format!("{nid:08x}"),
                                header: s.job.header.clone(),
                                height: s.job.height,
                                pool_target: new_target,
                                net_target: s.job.net_target,
                            };
                            registry.issue(derived.clone());
                            let _ = writeln!(stream.get_mut(), "{}", notify(&derived, false));
                        }
                        // Ack BEFORE the block submit RPC: the share is
                        // valid regardless of what the node says about a
                        // block, and a slow node must not push the ack
                        // past the miner's submit deadline (a timed-out
                        // miner reconnects mid-find and resubmits as a
                        // duplicate).
                        let _ = writeln!(
                            stream.get_mut(),
                            "{}",
                            response(&id, serde_json::Value::Bool(true))
                        );
                        if matches!(verdict, Verdict::Accept { block: true }) {
                            apply_block_found(&shared, miner_id.clone(), &s.job, s.nonce);
                        }
                    }
                    _ => {
                        shared
                            .stats
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .rejects += 1;
                        shared
                            .workers
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .rejected(&miner_id, now_secs());
                        let (code, text) = err.unwrap_or((-32600, "invalid submit".to_string()));
                        let _ = writeln!(stream.get_mut(), "{}", error_response(&id, code, &text));
                    }
                }
            }
            "" => {
                let _ = writeln!(
                    stream.get_mut(),
                    "{}",
                    error_response(&id, -32600, "missing method")
                );
            }
            other => {
                let _ = writeln!(
                    stream.get_mut(),
                    "{}",
                    error_response(&id, -32601, &format!("unknown method {other}"))
                );
            }
        }
    }
}

/// Issue a LuckyJob for a V1 template (fresh job id, shared registry
/// window) with an explicit pool target override, and return it. The
/// broadcast path passes the template target (or the vardiff override for
/// the current height); retargets pass the new target with `clean: false`.
/// Job ids stay unique per connection through the shared counter.
fn issue_lucky_job(
    counter: &AtomicU64,
    registry: &mut JobRegistry,
    channel_id: u32,
    template: &Job,
    pool_target: [u8; 32],
    clean: bool,
) -> LuckyJob {
    let job_id = counter.fetch_add(1, Ordering::SeqCst) as u32;
    registry.issue(Job {
        id: format!("v2-{job_id:08x}"),
        header: template.header.clone(),
        height: template.height,
        pool_target,
        net_target: template.net_target,
    });
    LuckyJob {
        channel_id,
        job_id,
        height: template.height,
        header: template.header.clone(),
        pool_target,
        net_target: template.net_target,
        clean,
    }
}

/// One open V2 standard channel on a connection: who it pays, its vardiff
/// controller and target override, and which broadcast version it has
/// seen. Channels are independent (a proxy can mine several workers down
/// one Noise session); the job registry and dedupe stay connection- and
/// pool-wide.
struct V2Channel {
    channel_id: u32,
    miner_id: String,
    vardiff: Vardiff,
    /// Active target override (target, template height).
    vd_target: Option<([u8; 32], u64)>,
    seen_version: u64,
    sent_height: u64,
    /// The miner's `OpenStandardMiningChannel.max_target`: the loosest
    /// target its hardware accepts. Everything this channel assigns is
    /// clamped to it (assigned = min(pool target, ceiling)); a target
    /// beyond the device's ability would be silently undiscardable work.
    max_target: [u8; 32],
}

/// Big-endian 256-bit min: the stricter (numerically smaller) target.
fn min_target(a: &[u8; 32], b: &[u8; 32]) -> [u8; 32] {
    if a <= b {
        *a
    } else {
        *b
    }
}

/// Send the current broadcast template as a [`LuckyJob`] when it is newer
/// than the channel's seen version. The vardiff override is honored only
/// while it names the current height (a new template resets the channel
/// to the template target, mirroring V1). Returns whether a job was sent.
/// The V1 [`JobRegistry`] is reused with `v2-{job_id:08x}` ids so
/// staleness, cross-connection dupe rejection and PoW evaluation are the
/// exact V1 rules — one validation path for both wires.
fn send_lucky_job<P: TemplateProvider + 'static, S: Read + Write>(
    session: &mut V2Session<S>,
    shared: &Shared<P>,
    registry: &mut JobRegistry,
    channel: &mut V2Channel,
) -> bool {
    let (cur, cur_version) = broadcast_job(shared);
    if cur_version == channel.seen_version {
        return false;
    }
    let clean = cur.height != channel.sent_height;
    let target = match channel.vd_target {
        Some((t, h)) if h == cur.height => min_target(&t, &channel.max_target),
        _ => min_target(&cur.pool_target, &channel.max_target),
    };
    let job = issue_lucky_job(
        &shared.job_counter,
        registry,
        channel.channel_id,
        &cur,
        target,
        clean,
    );
    if session.send(&to_frame(&job)).is_err() {
        return false;
    }
    channel.seen_version = cur_version;
    channel.sent_height = cur.height;
    true
}

/// V2 connection handler (behind `--v2-bind`): Noise NX handshake, version
/// negotiation, then any number of standard channels on the one encrypted
/// session: [`LuckyJob`] dispatch and [`LuckySubmit`] validation per
/// channel. Auth mirrors V1 (`auth_ok`): the worker name is
/// `OpenStandardMiningChannel.user_identity` and the pool password /
/// worker token rides in `SetupConnection.device_id` (the only free-form
/// client-chosen string in the V2 handshake). Each channel has its own
/// vardiff controller; the registry, dedupe, ledger and balances are
/// shared. Vardiff mirrors V1 (same controller, same 20 s/share target):
/// retargets send core `SetTarget` plus a fresh same-header job, and a new
/// template resets the channel to the template target.
fn handle_v2_conn<P: TemplateProvider + 'static>(
    stream: TcpStream,
    shared: Arc<Shared<P>>,
    authority: ([u8; 32], [u8; 32]),
    cert_validity: Duration,
) {
    let _ = stream.set_nodelay(true);
    eprintln!("pool(v2): connection from {:?}", stream.peer_addr());
    // Handshake bound: a port scan that connects and says nothing must not
    // park a thread forever.
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let mut session = match V2Session::accept(stream, authority, cert_validity) {
        Ok(s) => s,
        Err(e) => {
            // Loud: a failed handshake used to close silently, which made
            // client-side "connect failed" reports impossible to place.
            eprintln!("pool(v2): handshake failed: {e}");
            return;
        }
    };
    // Version negotiation: mining protocol, must span version 2. This
    // runs under the 10 s handshake timeout — NOT the 100 ms polling
    // timeout below: a slow client (a phone finishing the Noise math)
    // needs more than 100 ms to send its setup frame, and treating that
    // as an error closed the connection mid-negotiation.
    let setup = match accept_setup_connection(&mut session, |s| {
        if s.protocol != PROTOCOL_MINING || s.max_version < 2 || s.min_version > 2 {
            Err((
                0,
                "unsupported protocol/version (need mining v2)".to_string(),
            ))
        } else {
            Ok(SetupConnectionSuccess {
                used_version: 2,
                flags: crate::v2::FLAG_LUCKY_EXTENSION,
            })
        }
    }) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("pool(v2): setup negotiation failed: {e}");
            return;
        }
    };
    // From here the connection is in the polling loop: short reads let the
    // handler notice shutdown and fan out fresh jobs between receives.
    let _ = session.set_read_timeout(Some(Duration::from_millis(100)));
    // Open channels on this connection (usually one; a proxy may open
    // several workers down one Noise session). The registry window (16
    // jobs) is connection-wide and matches the V1 connection lifetime;
    // job ids are unique per connection through the shared counter.
    let mut registry = JobRegistry::new(16);
    let mut channels: HashMap<u32, V2Channel> = HashMap::new();

    loop {
        if shared.shutdown.load(Ordering::Relaxed) {
            break;
        }
        // Fresh jobs first, on every open channel: a rotation between the
        // last recv and this submit must reach the miner before its ack.
        for ch in channels.values_mut() {
            send_lucky_job(&mut session, &shared, &mut registry, ch);
        }
        // Timeout ticks are the job-poll heartbeat (same 100 ms cadence as
        // V1's read tick). Sends are single write_all calls on both sides,
        // so a timeout means silence, not a trickled frame — nothing
        // partial is lost by continuing here.
        let frame = match session.recv() {
            Ok(f) => f,
            Err(SessionError::Io(e))
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue
            }
            Err(_) => break,
        };
        match frame.msg_type {
            MSG_OPEN_STANDARD_MINING_CHANNEL => {
                let open = match from_frame::<OpenStandardMiningChannel>(&frame) {
                    Ok(o) => o,
                    Err(_) => continue,
                };
                if !auth_ok(
                    shared.worker_tokens.as_ref(),
                    shared.pool_password.as_deref(),
                    &open.user_identity,
                    &setup.device_id,
                ) {
                    // Refused, but the connection stays up: other channels
                    // (or a corrected retry) are unaffected.
                    let _ = session.send(&to_frame(&OpenMiningChannelError {
                        request_id: open.request_id,
                        error_code: "invalid credentials".to_string(),
                    }));
                    continue;
                }
                if open.user_identity.len() > MAX_WORKER_NAME_LEN {
                    let _ = session.send(&to_frame(&OpenMiningChannelError {
                        request_id: open.request_id,
                        error_code: "worker name too long (max 128 chars)".to_string(),
                    }));
                    continue;
                }
                if channels.len() >= MAX_CHANNELS_PER_SESSION {
                    let _ = session.send(&to_frame(&OpenMiningChannelError {
                        request_id: open.request_id,
                        error_code: "too many channels".to_string(),
                    }));
                    continue;
                }
                let miner_id = open.user_identity.clone();
                let mut agent = format!("v2 {} {}", setup.vendor, setup.hardware_version);
                agent.truncate(MAX_WORKER_NAME_LEN);
                shared
                    .workers
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .authorized(&miner_id, &agent, now_secs());
                let channel_id = shared.job_counter.fetch_add(1, Ordering::SeqCst) as u32;
                let (cur, _) = broadcast_job(&shared);
                // Honor the miner's max_target as a ceiling: never assign
                // looser work than the device asked to receive.
                let assigned = min_target(&cur.pool_target, &open.max_target);
                if session
                    .send(&to_frame(&OpenStandardMiningChannelSuccess {
                        request_id: open.request_id,
                        channel_id,
                        target: assigned,
                        extranonce_prefix: Vec::new(),
                        group_channel_id: 0,
                    }))
                    .is_err()
                {
                    break;
                }
                let mut ch = V2Channel {
                    channel_id,
                    miner_id,
                    vardiff: Vardiff::new(),
                    vd_target: None,
                    seen_version: 0,
                    sent_height: 0,
                    max_target: open.max_target,
                };
                // The broadcast always has a job (seeded at startup); a
                // failed first send means a dead peer.
                if !send_lucky_job(&mut session, &shared, &mut registry, &mut ch) {
                    break;
                }
                channels.insert(channel_id, ch);
            }
            MSG_LUCKY_SUBMIT => {
                let sub = match from_frame::<LuckySubmit>(&frame) {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                let Some(ch) = channels.get_mut(&sub.channel_id) else {
                    // Unknown (closed or never opened): say so explicitly
                    // and keep the connection up.
                    let nack = SubmitSharesError {
                        channel_id: sub.channel_id,
                        sequence_number: sub.sequence_number,
                        error_code: "unknown channel".to_string(),
                    };
                    if session.send(&to_frame(&nack)).is_err() {
                        break;
                    }
                    continue;
                };
                let channel_id = ch.channel_id;
                let miner_id = ch.miner_id.clone();
                let id = format!("v2-{:08x}", sub.job_id);
                let nonce_hex = format!("{:016x}", sub.nonce);
                let (verdict, submit) = { registry.evaluate(&id, &nonce_hex, &shared.dedupe) };
                let (accept, err) = verdict.response();
                match (accept, submit) {
                    (true, Some(s)) => {
                        let now = now_secs();
                        let work = work_for_share(&s.job.pool_target);
                        shared
                            .ledger
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .submit(ShareRec {
                                miner: miner_id.clone(),
                                ts: now,
                                work,
                                overnight: false,
                            });
                        shared
                            .stats
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .shares += 1;
                        shared
                            .workers
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .accepted(&miner_id, work, now);
                        let ack = SubmitSharesSuccess {
                            channel_id,
                            last_sequence_number: sub.sequence_number,
                            new_submits_accepted_count: 1,
                            new_shares_sum: work,
                        };
                        if session.send(&to_frame(&ack)).is_err() {
                            break;
                        }
                        // Vardiff: adapt this channel's pool target on the
                        // same template (mirrors V1). The miner keeps its
                        // nonce cursor: same header, fresh job id.
                        if let Some(new_work) = ch.vardiff.record(now, work) {
                            let new_target = min_target(&target_for_work(new_work), &ch.max_target);
                            ch.vd_target = Some((new_target, s.job.height));
                            if session
                                .send(&to_frame(&SetTarget {
                                    channel_id,
                                    maximum_target: new_target,
                                }))
                                .is_err()
                            {
                                break;
                            }
                            let job = issue_lucky_job(
                                &shared.job_counter,
                                &mut registry,
                                channel_id,
                                &s.job,
                                new_target,
                                false,
                            );
                            if session.send(&to_frame(&job)).is_err() {
                                break;
                            }
                        }
                        if matches!(verdict, Verdict::Accept { block: true }) {
                            apply_block_found(&shared, miner_id.clone(), &s.job, s.nonce);
                        }
                    }
                    _ => {
                        shared
                            .stats
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .rejects += 1;
                        shared
                            .workers
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .rejected(&miner_id, now_secs());
                        let (_, code) = err.unwrap_or((-32600, "invalid submit".to_string()));
                        let nack = SubmitSharesError {
                            channel_id,
                            sequence_number: sub.sequence_number,
                            error_code: code,
                        };
                        if session.send(&to_frame(&nack)).is_err() {
                            break;
                        }
                    }
                }
            }
            MSG_CLOSE_CHANNEL => {
                // Drop that one channel; the connection stays up — the
                // client may open another.
                if let Ok(close) = from_frame::<CloseChannel>(&frame) {
                    channels.remove(&close.channel_id);
                }
                continue;
            }
            // Ignore anything else (future extensions must not kill the
            // connection).
            _ => continue,
        }
    }
}

/// A running reference pool.
pub struct RunningServer<P: TemplateProvider + 'static> {
    addr: SocketAddr,
    shared: Arc<Shared<P>>,
    accept: Option<JoinHandle<()>>,
    refresh: Option<JoinHandle<()>>,
    /// V2 (Noise) accept loop; `None` until [`RunningServer::spawn_v2_listener`].
    v2_accept: Option<JoinHandle<()>>,
}

impl<P: TemplateProvider + 'static> RunningServer<P> {
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn stats(&self) -> Stats {
        self.stats_clone()
    }

    fn stats_clone(&self) -> Stats {
        self.shared
            .stats
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn blocks(&self) -> Vec<BlockRecord> {
        self.shared
            .blocks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .cloned()
            .collect()
    }

    /// Newest `n` block records (dashboard windows only ever show the
    /// tail). Clones O(n), not O(history) — `/api/stats` used to clone
    /// all 1000 records per poll to display 10.
    pub fn recent_blocks(&self, n: usize) -> Vec<BlockRecord> {
        let guard = self
            .shared
            .blocks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.iter().rev().take(n).rev().cloned().collect()
    }

    /// Ledger snapshot for `--state-file` persistence.
    pub fn ledger_snapshot(&self) -> PayoutLedger {
        self.shared
            .balances
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Restore a persisted ledger (replaces the fresh one).
    pub fn restore_ledger(&self, ledger: PayoutLedger) {
        *self
            .shared
            .balances
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = ledger;
    }

    /// Share-ledger snapshot for `--state-file` persistence (streaks,
    /// division boards and PPLNS windows survive pool restarts).
    pub fn shares_snapshot(&self) -> Ledger {
        self.shared
            .ledger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Restore persisted shares (replaces the fresh share ledger).
    pub fn restore_shares(&self, mut ledger: Ledger) {
        // Pre-rollup state files carry full raw history but empty tables:
        // rebuild once so the boards keep working across the upgrade.
        // (New files already carry the tables; the rebuild then no-ops.)
        ledger.rebuild_rollups();
        *self
            .shared
            .ledger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = ledger;
    }

    /// Persisted worker-name map (miner id → payout address).
    pub fn names(&self) -> Vec<(String, String)> {
        self.shared
            .workers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .names()
    }

    /// Restore persisted worker names (payout destinations survive a
    /// restart even before miners reconnect).
    pub fn restore_names(&self, names: &[(String, String)]) {
        self.shared
            .workers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .restore_names(names);
    }

    /// Last-known agent per miner (live + restored) for division resolution.
    pub fn agents(&self) -> Vec<(String, String)> {
        self.shared
            .workers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .agents()
    }

    /// Restore persisted agents (idle miners keep their declared family).
    pub fn restore_agents(&self, agents: &[(String, String)]) {
        self.shared
            .workers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .restore_agents(agents);
    }

    /// Restore persisted block history; the lifetime counter follows so
    /// the dashboard reads continuously across restarts.
    pub fn restore_blocks(&self, blocks: Vec<BlockRecord>) {
        let n = blocks.len() as u64;
        let mut b = self
            .shared
            .blocks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        b.clear();
        b.extend(blocks);
        drop(b);
        let mut st = self
            .shared
            .stats
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        st.blocks = st.blocks.max(n);
    }

    pub fn aggregate(&self, window_secs: u64, now: u64) -> Vec<MinerAgg> {
        self.shared
            .ledger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .aggregate(window_secs, now)
    }

    /// Full-history streak state per miner (doc 07 §7.4 dashboard tile):
    /// the compacted day rollup only — O(miners), safe on every stats poll.
    pub fn streaks(&self, now: u64) -> Vec<lm_pool::ledger::StreakState> {
        self.shared
            .ledger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .streaks(now)
    }

    /// Seasonal division board over `window_secs` (doc 03 §3.3), effective
    /// work desc. O(miners) over the share log's retention window.
    #[must_use]
    pub fn leaderboard(&self, window_secs: u64, now: u64) -> Vec<lm_pool::ledger::BoardEntry> {
        self.shared
            .ledger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .leaderboard(window_secs, now)
    }

    /// In-flight faucet claims for the top-up policy (state-file round
    /// trip — a restart must not forget unmatured claims and re-claim a
    /// full drip on the next template).
    pub fn faucet_unmatured(&self) -> Vec<(u64, u64)> {
        self.shared
            .provider
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .faucet_unmatured_snapshot()
    }

    /// Restore persisted in-flight faucet claims (stale entries prune on
    /// the next template build).
    pub fn restore_faucet_unmatured(&self, claimed: Vec<(u64, u64)>) {
        self.shared
            .provider
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .restore_faucet_unmatured(claimed);
    }

    /// Per-worker snapshots for the dashboard/API.
    /// Workers idle past `WORKER_HIDE_SECS` (6 h) drop off this list; the
    /// unfiltered `snapshot`/`names` still back accounting and payouts.
    pub fn workers(&self, now: u64) -> Vec<WorkerSnapshot> {
        self.shared
            .workers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .snapshot_visible(now)
    }

    /// All miner balances (owed) for the dashboard/API, miner-ascending.
    /// Payout selection itself lives in the payout path.
    pub fn balances(&self) -> Vec<(String, u64)> {
        self.shared
            .balances
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .select_due(1)
            .into_iter()
            .collect()
    }

    /// Debit after a successful on-chain send.
    pub fn record_paid(&self, miner: &str, amount: u64) {
        self.shared
            .balances
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .record_paid(miner, amount);
    }

    /// Lifetime paid to one miner (stats reconciliation).
    pub fn paid(&self, miner: &str) -> u64 {
        self.shared
            .balances
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .paid(miner)
    }

    /// One miner's row (worker stats + owed/paid). `None` when unknown.
    #[must_use]
    pub fn miner_row(&self, miner: &str, now: u64) -> Option<MinerRow> {
        let snap = self
            .shared
            .workers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .snapshot(now)
            .into_iter()
            .find(|w| w.miner == miner)?;
        let bal = self
            .shared
            .balances
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Some(MinerRow {
            miner: miner.to_string(),
            name: snap.name,
            agent: snap.agent,
            shares: snap.shares,
            rejects: snap.rejects,
            online: snap.online,
            last_seen: snap.last_seen,
            rates: snap.rates,
            owed: bal.owed(miner),
            paid: bal.paid(miner),
        })
    }

    /// Per-miner payout settings snapshot (persisted with the state file).
    #[must_use]
    pub fn payout_settings(&self) -> PayoutSettings {
        self.shared
            .payout_settings
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Replace the payout settings (state restore).
    pub fn restore_payout_settings(&self, settings: PayoutSettings) {
        *self
            .shared
            .payout_settings
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = settings;
    }

    /// Set one miner's payout settings (replaces the whole entry).
    pub fn set_payout_settings(&self, miner: String, settings: MinerSettings) {
        self.shared
            .payout_settings
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(miner, settings);
    }

    /// Remove one miner's settings entirely. Returns whether one existed.
    pub fn clear_payout_settings(&self, miner: &str) -> bool {
        self.shared
            .payout_settings
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(miner)
            .is_some()
    }

    /// Attestation snapshot (persisted with the state file).
    #[must_use]
    pub fn attestations(&self) -> Attestations {
        self.shared
            .attestations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Replace the attestation store (state restore).
    pub fn restore_attestations(&self, attestations: Attestations) {
        *self
            .shared
            .attestations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = attestations;
    }

    /// Record one worker's attestation verdict (latest wins).
    pub fn set_attestation(&self, miner: String, attestation: Attestation) {
        self.shared
            .attestations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(miner, attestation);
    }

    /// The current (non-expired) attestation for one miner.
    #[must_use]
    pub fn attestation_for(&self, miner: &str, now: u64) -> Option<Attestation> {
        let all = self
            .shared
            .attestations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        all.get(miner)
            .filter(|a| now.saturating_sub(a.time) <= ATTESTATION_TTL_SECS)
            .cloned()
    }

    /// Miner id owning a worker token (`--workers-file`), for the
    /// miner-facing HTTP endpoints. `None` when auth is off or the token is
    /// unknown.
    #[must_use]
    pub fn miner_for_token(&self, token: &str) -> Option<String> {
        if token.is_empty() {
            return None;
        }
        let tokens = self.shared.worker_tokens.as_ref()?;
        let worker = tokens
            .iter()
            .find(|(_, t)| ct_eq(t, token))
            .map(|(w, _)| w.as_str())?;
        self.shared
            .workers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .miner_for_name(worker)
    }

    /// Whether a `--workers-file` token map is active (auth required).
    #[must_use]
    pub fn worker_tokens_active(&self) -> bool {
        self.shared.worker_tokens.is_some()
    }

    /// `(owed_total, paid_total, fees)` — the payout reconciliation triple.
    pub fn payout_totals(&self) -> (u64, u64, u64) {
        self.shared
            .balances
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .totals()
    }

    /// Lifetime miner-owed coinbase accepted (normal value + jackpot
    /// bonuses + pot splashes). Must equal owed + paid + fees at all
    /// times — the /api/stats identity.
    pub fn credited_total(&self) -> u64 {
        *self
            .shared
            .credited_total
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Restore the persisted lifetime coinbase total (state-file load).
    pub fn restore_credited(&self, total: u64) {
        *self
            .shared
            .credited_total
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = total;
    }

    /// Pool fee in per-myriad for block credits (0 = none).
    pub fn set_pool_fee(&self, fee_permyriad: u16) {
        *self
            .shared
            .pool_fee
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = fee_permyriad;
    }

    /// Dev-fee slice (docs/04 §4.2): `pp` per-myriad of each block's
    /// coinbase moves from the operator fee pot to `addr`'s balance. The
    /// address is registered as a payout name so the payout loop pays it
    /// like any worker. No-op when `addr` is empty or `pp` is 0.
    pub fn set_dev_fee(&self, addr: &str, pp: u16) {
        if addr.is_empty() || pp == 0 {
            return;
        }
        // The dev worker is a name like any other (money identity).
        let id = addr.to_string();
        *self
            .shared
            .dev_fee
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((id.clone(), pp));
        self.restore_names(&[(id, addr.to_string())]);
    }

    /// Per-height prize inflow this pool has credited (height, amount),
    /// ascending. Covers blocks the pool did NOT mine (staker/peer
    /// finds) — those never enter the block history. Persisted across
    /// restarts; bounded to the recent window.
    pub fn prize_credits(&self) -> Vec<(u64, u64)> {
        self.shared
            .prize_credit_log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(|(h, a)| (*h, *a))
            .collect()
    }

    /// Restore the persisted per-height prize credits (state-file load).
    pub fn restore_prize_credits(&self, credits: &[(u64, u64)]) {
        let mut log = self
            .shared
            .prize_credit_log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        log.clear();
        log.extend(credits.iter().copied());
    }

    /// Current-epoch Luckynode Draw advisory for the dashboard card.
    pub fn latest_draw(&self) -> DrawStatus {
        self.shared
            .latest_draw
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Signal shutdown and wait for the accept loop (connection threads exit
    /// on their next 100 ms tick) and the refresh loop.
    pub fn stop(mut self) {
        self.shared.shutdown.store(true, Ordering::SeqCst);
        if let Some(h) = self.accept.take() {
            let _ = h.join();
        }
        if let Some(h) = self.v2_accept.take() {
            let _ = h.join();
        }
        if let Some(h) = self.refresh.take() {
            let _ = h.join();
        }
    }

    /// Serve Stratum V2 (Noise NX + Lucky jobs) on `addr`, sharing this
    /// server's provider, ledger, broadcast, dedupe and balances — one
    /// money path for V1 and V2 miners. Behind the pool's `--v2-bind`
    /// A second V1 listener on the SAME shared state (ledger, dedupe,
    /// caps, broadcast) with its own TLS treatment — e.g. plaintext
    /// :3346 plus TLS-only :3443. The accept loop is shared, so caps and
    /// shutdown behave identically on both binds. Detached thread; dies
    /// with the process (pool binaries never shut down one listener).
    pub fn spawn_extra_listener(
        &self,
        addr: &str,
        tls: Option<Arc<rustls::ServerConfig>>,
    ) -> std::io::Result<SocketAddr> {
        let listener = TcpListener::bind(addr)?;
        listener.set_nonblocking(true)?;
        let local = listener.local_addr()?;
        let shared = self.shared.clone();
        std::thread::spawn(move || accept_loop(listener, shared, tls));
        Ok(local)
    }

    /// flag; V1 stays the default wire.
    ///
    /// # Errors
    /// When `addr` cannot be bound.
    pub fn spawn_v2_listener(
        &mut self,
        addr: &str,
        authority: ([u8; 32], [u8; 32]),
        cert_validity: Duration,
    ) -> std::io::Result<SocketAddr> {
        let listener = TcpListener::bind(addr)?;
        listener.set_nonblocking(true)?;
        let local = listener.local_addr()?;
        let shared = Arc::clone(&self.shared);
        let handle = std::thread::spawn(move || {
            use std::sync::atomic::Ordering as AtomicOrdering;
            while !shared.shutdown.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, peer)) => {
                        // Same caps as the V1 accept loop, against the SAME
                        // counters: V1 and V2 sockets draw on one budget,
                        // so a flood on one wire cannot bypass the other's
                        // cap (audit finding: the V2 listener used to
                        // spawn a thread per connection with no bound —
                        // thread exhaustion + one Noise handshake's CPU
                        // per socket on a public port).
                        let ip = peer.ip();
                        let total = shared.conn_total.load(AtomicOrdering::SeqCst);
                        let per_ip = shared
                            .conn_per_ip
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .get(&ip)
                            .copied()
                            .unwrap_or(0);
                        if total >= MAX_CONNS || per_ip >= MAX_CONNS_PER_IP {
                            drop(stream);
                            continue;
                        }
                        shared.conn_total.fetch_add(1, AtomicOrdering::SeqCst);
                        {
                            let mut m = shared
                                .conn_per_ip
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            *m.entry(ip).or_insert(0) += 1;
                        }
                        let s = shared.clone();
                        std::thread::spawn(move || {
                            // RAII: the counters release on every exit path.
                            let _guard = ConnGuard {
                                shared: s.clone(),
                                ip,
                            };
                            handle_v2_conn(stream, s, authority, cert_validity)
                        });
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    Err(e) => {
                        eprintln!("pool(v2): accept error (continuing): {e}");
                        std::thread::sleep(Duration::from_millis(20));
                    }
                }
            }
        });
        self.v2_accept = Some(handle);
        Ok(local)
    }
}

impl RunningServer<crate::provider::MockProvider> {
    pub fn chain_height(&self) -> u64 {
        self.shared
            .provider
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .height()
    }

    pub fn reserve(&self) -> u64 {
        self.shared
            .provider
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .reserve()
    }

    pub fn chain_blocks(&self) -> u64 {
        self.shared
            .provider
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .blocks()
    }
}

/// Bind on `addr` (use `127.0.0.1:0` for an ephemeral test port) and serve.
/// V1 accept loop shared by the main listener and any extra binds.
/// `tls_override`: the extra listener's own TLS treatment (`Some` =
/// TLS with this config); `None` means "use the shared config", which
/// is what the main listener (and all pre-TLS-bind behavior) does.
/// Connection caps and per-connection threads are shared across every
/// listener — one budget for the whole pool, whichever port they came in
/// on (audit finding: uncapped listeners are a slowloris well).
fn accept_loop<P: TemplateProvider + 'static>(
    listener: TcpListener,
    accept_shared: Arc<Shared<P>>,
    tls_override: Option<Arc<rustls::ServerConfig>>,
) {
    use std::sync::atomic::Ordering as AtomicOrdering;
    while !accept_shared.shutdown.load(AtomicOrdering::Relaxed) {
        match listener.accept() {
            Ok((stream, peer)) => {
                // Caps: bounded threads per
                // pool, bounded share per IP. Over-cap connections are
                // dropped at accept — no thread, no TLS, no state.
                let ip = peer.ip();
                let total = accept_shared.conn_total.load(AtomicOrdering::SeqCst);
                let per_ip = accept_shared
                    .conn_per_ip
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get(&ip)
                    .copied()
                    .unwrap_or(0);
                if total >= MAX_CONNS || per_ip >= MAX_CONNS_PER_IP {
                    drop(stream);
                    continue;
                }
                accept_shared
                    .conn_total
                    .fetch_add(1, AtomicOrdering::SeqCst);
                {
                    let mut m = accept_shared
                        .conn_per_ip
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    *m.entry(ip).or_insert(0) += 1;
                }
                let s = accept_shared.clone();
                let tls = tls_override.clone();
                std::thread::spawn(move || {
                    let _guard = ConnGuard {
                        shared: s.clone(),
                        ip,
                    };
                    handle_conn(stream, s, tls.as_ref());
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => {
                // A transient accept error (EMFILE, ECONNABORTED, ...)
                // must not deafen the pool: the kernel completes
                // handshakes into the listen backlog without the
                // application, so exiting this loop would turn every
                // later connect into a silent hang — the client
                // connects, then times out with no server thread ever
                // spawned. Log it and keep serving instead.
                eprintln!("pool: accept error (continuing): {e}");
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

pub fn spawn_server<P: TemplateProvider + 'static>(
    provider: P,
    addr: &str,
    password: Option<String>,
    worker_tokens: Option<HashMap<String, String>>,
    tls: Option<Arc<rustls::ServerConfig>>,
) -> std::io::Result<RunningServer<P>> {
    let listener = TcpListener::bind(addr)?;
    listener.set_nonblocking(true)?;
    let local = listener.local_addr()?;
    let tpl = provider.current();
    // Draw advisory for the dashboard card (moved fields below borrow
    // first — header is moved into the seed job).
    let draw = draw_status_of(&tpl);
    let job = Job {
        id: "00000000".to_string(),
        header: tpl.header,
        height: tpl.height,
        pool_target: tpl.pool_target,
        net_target: tpl.net_target,
    };
    let shared = Arc::new(Shared {
        provider: Mutex::new(provider),
        ledger: Mutex::new(Ledger::new()),
        blocks: Mutex::new(VecDeque::new()),
        stats: Mutex::new(Stats::default()),
        broadcast: Mutex::new(JobState { job, version: 1 }),
        dedupe: Mutex::new(Dedupe::default()),
        tip_overflow: Mutex::new(BTreeMap::new()),
        prize_credited: Mutex::new(HashSet::new()),
        prize_credit_log: Mutex::new(BTreeMap::new()),
        prize_tip: Mutex::new(0),
        latest_draw: Mutex::new(draw),
        workers: Mutex::new(WorkerStats::default()),
        balances: Mutex::new(PayoutLedger::default()),
        credited_total: Mutex::new(0),
        payout_settings: Mutex::new(PayoutSettings::new()),
        attestations: Mutex::new(Attestations::new()),
        pool_fee: Mutex::new(0),
        dev_fee: Mutex::new(None),
        pool_password: password,
        worker_tokens,
        tls,
        job_counter: AtomicU64::new(1),
        shutdown: AtomicBool::new(false),
        conn_total: std::sync::atomic::AtomicUsize::new(0),
        conn_per_ip: Mutex::new(std::collections::HashMap::new()),
    });
    // Seed the prospective prize map with the startup template: the
    // first block after a (re)start would otherwise have no record and
    // an external find's overflow would leak through the reconcile.
    {
        let mut pend = shared
            .tip_overflow
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pend.insert(tpl.height, tpl.prize);
        *shared
            .prize_tip
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = tpl.height.saturating_sub(1);
    }
    let accept_shared = shared.clone();
    let accept = std::thread::spawn(move || {
        accept_loop(listener, accept_shared, None);
    });
    let refresh_shared = shared.clone();
    let refresh = std::thread::spawn(move || refresh_loop(refresh_shared));
    Ok(RunningServer {
        addr: local,
        shared,
        accept: Some(accept),
        refresh: Some(refresh),
        v2_accept: None,
    })
}

#[cfg(test)]
mod auth_tests {
    use super::*;

    fn tokens() -> HashMap<String, String> {
        let mut m = HashMap::new();
        m.insert("worker-a".to_string(), "tok-a".to_string());
        m.insert("worker-b".to_string(), "tok-b".to_string());
        m
    }

    #[test]
    fn tokens_are_authoritative_when_set() {
        let t = tokens();
        assert!(auth_ok(Some(&t), None, "worker-a", "tok-a"));
        assert!(!auth_ok(Some(&t), None, "worker-a", "wrong"));
        assert!(!auth_ok(Some(&t), None, "unknown", "tok-a"));
        assert!(!auth_ok(Some(&t), None, "worker-a", ""));
        // Even a shared password does not bypass the token allowlist.
        assert!(!auth_ok(Some(&t), Some("shared"), "worker-a", "shared"));
    }

    #[test]
    fn shared_password_fallback_and_open_mode() {
        assert!(auth_ok(None, Some("pw"), "anyone", "pw"));
        assert!(!auth_ok(None, Some("pw"), "anyone", "nope"));
        assert!(auth_ok(None, None, "anyone", ""));
        assert!(auth_ok(None, None, "anyone", "anything"));
    }

    #[test]
    fn workers_file_parses_and_validates() {
        let m =
            parse_workers_file("# comment\n\nworker-a: tok-a\nworker-b:tok:b\n").expect("parse");
        assert_eq!(m.get("worker-a").map(String::as_str), Some("tok-a"));
        // Token keeps colons after the first separator.
        assert_eq!(m.get("worker-b").map(String::as_str), Some("tok:b"));

        assert!(parse_workers_file("no-colon-line").is_err());
        assert!(parse_workers_file("worker:").is_err());
        assert!(parse_workers_file(":token").is_err());
        assert!(parse_workers_file("w:1\nw:2").is_err());
    }

    /// Regression (found in testing with pcaps on testnet): a message split
    /// across the 100 ms read tick — submit body, then the newline after a
    /// Nagle-sized gap — must still be answered. The old clear-then-read
    /// pattern discarded the partial line, the miner never got a response,
    /// timed out and reconnected in a loop.
    #[test]
    fn split_line_across_read_tick_is_not_discarded() {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpStream;
        use std::time::Duration;

        let provider = crate::provider::MockProvider::new([0xffu8; 32], [0x00u8; 32], 1_000_000, 0);
        let server = spawn_server(provider, "127.0.0.1:0", None, None, None).expect("spawn");
        let stream = TcpStream::connect(server.addr).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .expect("timeout");
        let mut r = BufReader::new(stream.try_clone().expect("clone"));
        let mut w = stream;

        w.write_all(b"{\"id\":1,\"method\":\"mining.subscribe\",\"params\":[]}\n")
            .expect("subscribe");
        w.write_all(b"{\"id\":2,\"method\":\"mining.authorize\",\"params\":[\"split\",\"\"]}\n")
            .expect("authorize");

        // Drain up to the authorize response (notify may follow it).
        let mut saw_authorize = false;
        let mut line = String::new();
        while !saw_authorize {
            line.clear();
            let n = r.read_line(&mut line).expect("read");
            assert!(n > 0, "server closed before authorize response");
            saw_authorize = line.contains("\"id\":2");
        }

        // The bug: body and newline in separate writes >100 ms apart.
        w.write_all(
            b"{\"id\":3,\"method\":\"mining.submit\",\"params\":[\"split\",\"x\",\"0000000000000001\"]}",
        )
        .expect("submit body");
        std::thread::sleep(Duration::from_millis(150));
        w.write_all(b"\n").expect("submit newline");

        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while std::time::Instant::now() < deadline {
            line.clear();
            match r.read_line(&mut line) {
                Ok(0) => panic!("server closed before submit response"),
                Ok(_) => {
                    if line.contains("\"id\":3") {
                        return; // answered — bug fixed
                    }
                }
                Err(_) => break, // timeout
            }
        }
        panic!("split submit got no response within 3 s (partial line discarded)");
    }

    /// Miner-facing auth + payout settings: a worker token
    /// resolves to the miner id only after authorize, and settings
    /// round-trip through the server.
    #[test]
    fn miner_token_maps_to_the_worker_and_settings_round_trip() {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpStream;
        use std::time::Duration;

        let provider = crate::provider::MockProvider::new([0xffu8; 32], [0x00u8; 32], 1_000_000, 0);
        let mut toks = HashMap::new();
        toks.insert("worker-a".to_string(), "tok-a".to_string());
        let server = spawn_server(provider, "127.0.0.1:0", None, Some(toks), None).expect("spawn");
        // No worker has connected: the token cannot resolve yet.
        assert_eq!(server.miner_for_token("tok-a"), None);
        assert_eq!(server.miner_for_token(""), None);

        let stream = TcpStream::connect(server.addr).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .expect("timeout");
        let mut r = BufReader::new(stream.try_clone().expect("clone"));
        let mut w = stream;
        w.write_all(b"{\"id\":1,\"method\":\"mining.subscribe\",\"params\":[]}\n")
            .expect("subscribe");
        w.write_all(
            b"{\"id\":2,\"method\":\"mining.authorize\",\"params\":[\"worker-a\",\"tok-a\"]}\n",
        )
        .expect("authorize");
        let mut saw_authorize = false;
        let mut line = String::new();
        while !saw_authorize {
            line.clear();
            let n = r.read_line(&mut line).expect("read");
            assert!(n > 0, "server closed before authorize response");
            saw_authorize = line.contains("\"id\":2");
        }
        let miner = server
            .miner_for_token("tok-a")
            .expect("token resolves to the miner");
        assert!(server.miner_for_token("wrong").is_none());

        // Settings round-trip + clear.
        server.set_payout_settings(
            miner.clone(),
            MinerSettings {
                address: Some("addr-x".to_string()),
                threshold: Some(42),
            },
        );
        let s = server.payout_settings();
        assert_eq!(
            s.get(miner.as_str()).and_then(|s| s.address.as_deref()),
            Some("addr-x")
        );
        assert_eq!(s.get(miner.as_str()).and_then(|s| s.threshold), Some(42));
        assert!(server.clear_payout_settings(&miner));
        assert!(!server.clear_payout_settings(&miner));
        assert!(server.payout_settings().is_empty());

        // Attestation store: latest wins, expiry filters, restore replaces.
        let now = now_secs();
        server.set_attestation(
            miner.clone(),
            Attestation {
                provider: "self".to_string(),
                time: now,
                detail: "test-device".to_string(),
            },
        );
        let att = server
            .attestation_for(&miner, now)
            .expect("fresh attestation");
        assert_eq!(att.provider, "self");
        assert!(server
            .attestation_for(&miner, now + ATTESTATION_TTL_SECS + 1)
            .is_none());
        assert_eq!(server.attestations().len(), 1);
        server.restore_attestations(Attestations::new());
        assert!(server.attestation_for(&miner, now).is_none());

        // The miner row is visible once connected.
        let row = server.miner_row(&miner, now_secs()).expect("row");
        assert_eq!(row.name, "worker-a");
        assert_eq!(row.shares, 0);
        assert_eq!(row.owed, 0);
        assert_eq!(row.paid, 0);
        server.stop();
    }

    /// V2 vardiff (mirrors V1 pacing): 8 fast shares on a static template
    /// retarget the channel harder via core `SetTarget` plus a fresh
    /// same-header job carrying the new target.
    #[test]
    fn v2_vardiff_retargets_with_set_target_and_job() {
        use crate::v2::{
            LuckyJob, LuckySubmit, OpenStandardMiningChannel, OpenStandardMiningChannelSuccess,
            SetTarget, SetupConnection, SubmitSharesSuccess, MSG_LUCKY_JOB, MSG_SET_TARGET,
            MSG_SUBMIT_SHARES_SUCCESS, PROTOCOL_MINING,
        };
        use std::net::TcpStream;
        use std::time::Duration;

        // Tight net target: shares accept, never blocks → height static.
        let provider = crate::provider::MockProvider::new([0xffu8; 32], [0x00u8; 32], 1_000_000, 0);
        let mut server = spawn_server(provider, "127.0.0.1:0", None, None, None).expect("spawn");
        let authority = crate::v2_session::generate_authority();
        let v2_addr = server
            .spawn_v2_listener("127.0.0.1:0", authority, Duration::from_secs(3600))
            .expect("v2 listen");
        let stream = TcpStream::connect(v2_addr).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
        let mut session =
            crate::v2_session::V2Session::connect(stream, Some(authority.0)).expect("handshake");
        crate::v2_session::setup_connection(
            &mut session,
            &SetupConnection {
                protocol: PROTOCOL_MINING,
                min_version: 2,
                max_version: 2,
                flags: 0,
                endpoint_host: "x".to_string(),
                endpoint_port: 1,
                vendor: "t".to_string(),
                hardware_version: "cpu".to_string(),
                firmware: "0.2".to_string(),
                device_id: "x".to_string(),
            },
        )
        .expect("negotiate");
        session
            .send(&crate::v2::to_frame(&OpenStandardMiningChannel {
                request_id: 1,
                user_identity: "vd-miner".to_string(),
                nominal_hash_rate: 0.0,
                max_target: [0xffu8; 32],
            }))
            .expect("open");
        let opened = crate::v2::from_frame::<OpenStandardMiningChannelSuccess>(
            &session.recv().expect("open success"),
        )
        .expect("decode open");
        let channel_id = opened.channel_id;
        let first_target = opened.target;
        let job_frame = session.recv().expect("job");
        assert_eq!(job_frame.msg_type, MSG_LUCKY_JOB);
        let job = LuckyJob::from_frame(&job_frame).expect("decode job");
        assert_eq!(job.pool_target, first_target);
        // 8 fast shares, distinct nonces; the ack always arrives before
        // any retarget message (the server sends it first).
        for i in 0..8u64 {
            session
                .send(&crate::v2::to_frame(&LuckySubmit {
                    channel_id,
                    job_id: job.job_id,
                    sequence_number: i as u32,
                    nonce: i,
                }))
                .expect("submit");
            let ack_frame = session.recv().expect("ack");
            assert_eq!(ack_frame.msg_type, MSG_SUBMIT_SHARES_SUCCESS, "submit {i}");
            let ack = SubmitSharesSuccess::from_frame(&ack_frame).expect("decode ack");
            assert_eq!(ack.last_sequence_number, i as u32);
        }
        // Retarget: SetTarget first, then a same-header job with the target.
        let st_frame = session.recv().expect("settarget");
        assert_eq!(st_frame.msg_type, MSG_SET_TARGET);
        let st = SetTarget::from_frame(&st_frame).expect("decode target");
        assert_eq!(st.channel_id, channel_id);
        assert!(
            st.maximum_target < first_target,
            "8 instant shares must step harder"
        );
        let job2_frame = session.recv().expect("job2");
        assert_eq!(job2_frame.msg_type, MSG_LUCKY_JOB);
        let job2 = LuckyJob::from_frame(&job2_frame).expect("decode job2");
        assert_eq!(job2.header, job.header, "same template, new target");
        assert_eq!(job2.pool_target, st.maximum_target);
        assert!(!job2.clean, "same height is not clean");
        server.stop();
    }

    /// Several channels share one Noise session: two workers open, submit,
    /// and accrue independently; closing one channel answers later submits
    /// with "unknown channel" while the other keeps mining.
    /// Regression (audit finding: unbounded `line` growth): a peer that
    /// streams bytes WITHOUT ever sending a newline used to grow the
    /// partial-line buffer past MAX_LINE_BYTES unchecked — the cap only
    /// ran after a complete line. The pool must disconnect it instead of
    /// letting memory grow without bound.
    #[test]
    fn newlineless_flood_is_disconnected_at_the_line_cap() {
        use std::io::{Read, Write};
        use std::net::TcpStream;
        use std::time::Duration;

        let provider = crate::provider::MockProvider::new([0xffu8; 32], [0x00u8; 32], 1_000_000, 0);
        let server = spawn_server(provider, "127.0.0.1:0", None, None, None).expect("spawn");
        let mut stream = TcpStream::connect(server.addr()).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");

        // Authorize first (the guard applies at every tick either way, but
        // this proves it is not an auth-deadline disconnect).
        stream
            .write_all(b"{\"id\":1,\"method\":\"mining.authorize\",\"params\":[\"flood\",\"\"]}\n")
            .expect("authorize");

        // More than MAX_LINE_BYTES with no newline, in dribbles: every
        // write lands below the read tick's appetite, so the server sees
        // only ever-growing partials.
        let chunk = [b'x'; 8 * 1024];
        for _ in 0..10 {
            stream.write_all(&chunk).expect("dribble");
            std::thread::sleep(Duration::from_millis(120));
        }

        // The server must hit the cap and close the connection. Some
        // frames are already in flight (authorize response + notify), so
        // drain: every read must make progress toward EOF, never stall.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut buf = [0u8; 4096];
        loop {
            if std::time::Instant::now() > deadline {
                panic!("server still talking to a newlineless flood");
            }
            match stream.read(&mut buf) {
                Ok(0) => break, // clean EOF — the guard tripped
                Ok(_) => continue,
                Err(e) => panic!("expected EOF after the line cap, got {e}"),
            }
        }
        drop(stream);
        server.stop();
    }

    /// Regression (audit finding: V2 had no connection caps). The V2
    /// listener draws on the same MAX_CONNS / MAX_CONNS_PER_IP budget as
    /// V1 against the same counters; a per-IP flood is dropped at accept.
    /// (2048 total sockets is too heavy for a unit test; the per-IP cap
    /// is the one exercised here with loopback sockets.)
    #[test]
    fn v2_flood_beyond_the_per_ip_cap_is_dropped() {
        use std::io::Read;
        use std::net::TcpStream;
        use std::time::Duration;

        let provider = crate::provider::MockProvider::new([0xffu8; 32], [0x00u8; 32], 1_000_000, 0);
        let mut server = spawn_server(provider, "127.0.0.1:0", None, None, None).expect("spawn");
        let authority = crate::v2_session::generate_authority();
        let v2_addr = server
            .spawn_v2_listener("127.0.0.1:0", authority, Duration::from_secs(3600))
            .expect("v2 listen");

        // Park MAX_CONNS_PER_IP sockets mid-handshake (connected, no
        // bytes: each holds a thread inside its handshake timeout).
        let mut parked = Vec::new();
        for _ in 0..MAX_CONNS_PER_IP {
            parked.push(TcpStream::connect(v2_addr).expect("park connect"));
        }
        std::thread::sleep(Duration::from_millis(300));

        // The next one from the same IP is dropped at accept: closed
        // without a handshake reply. A kept socket would sit silent for
        // its whole 10 s handshake window; the dropped one EOFs fast.
        let mut extra = TcpStream::connect(v2_addr).expect("extra connect");
        extra
            .set_read_timeout(Some(Duration::from_secs(3)))
            .expect("timeout");
        let mut buf = [0u8; 1];
        match extra.read(&mut buf) {
            Ok(0) => {} // dropped at accept, as designed
            Ok(_) => panic!("v2 accepted beyond the per-IP cap"),
            Err(e) => panic!("expected the over-cap v2 socket to drop, got {e}"),
        }
        drop(parked);
        drop(extra);
        server.stop();
    }

    /// Regression (audit finding: uncapped channels per session). Opening
    /// past MAX_CHANNELS_PER_SESSION answers OpenMiningChannelError and
    /// keeps the session alive.
    #[test]
    fn v2_open_beyond_the_channel_cap_is_refused() {
        use crate::v2::{
            OpenMiningChannelError, OpenStandardMiningChannel, SetupConnection, PROTOCOL_MINING,
        };
        use std::net::TcpStream;
        use std::time::Duration;

        let provider = crate::provider::MockProvider::new([0xffu8; 32], [0x00u8; 32], 1_000_000, 0);
        let mut server = spawn_server(provider, "127.0.0.1:0", None, None, None).expect("spawn");
        let authority = crate::v2_session::generate_authority();
        let v2_addr = server
            .spawn_v2_listener("127.0.0.1:0", authority, Duration::from_secs(3600))
            .expect("v2 listen");
        let stream = TcpStream::connect(v2_addr).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
        let mut session =
            crate::v2_session::V2Session::connect(stream, Some(authority.0)).expect("handshake");
        crate::v2_session::setup_connection(
            &mut session,
            &SetupConnection {
                protocol: PROTOCOL_MINING,
                min_version: 2,
                max_version: 2,
                flags: 0,
                endpoint_host: "x".to_string(),
                endpoint_port: 1,
                vendor: "t".to_string(),
                hardware_version: "cpu".to_string(),
                firmware: "0.2".to_string(),
                device_id: "x".to_string(),
            },
        )
        .expect("negotiate");

        // Fill the session to the cap: each open answers success + one
        // LuckyJob. (Noise handshake CPU per channel is the real-world
        // cost; here we only need the frames.)
        let mut last_req = 0u32;
        for i in 0..MAX_CHANNELS_PER_SESSION {
            let req = (i + 1) as u32;
            session
                .send(&crate::v2::to_frame(&OpenStandardMiningChannel {
                    request_id: req,
                    user_identity: format!("cap-{req}"),
                    nominal_hash_rate: 0.0,
                    max_target: [0xffu8; 32],
                }))
                .expect("open");
            // Drain this channel's success + first job so the refusal
            // below reads the very next frame deterministically.
            let _ = crate::v2::from_frame::<crate::v2::OpenStandardMiningChannelSuccess>(
                &session.recv().expect("open success"),
            )
            .expect("decode success");
            let _job = session.recv().expect("job");
            last_req = req;
        }

        // One past the cap: an explicit error, and the session stays up
        // (a later valid frame still answers).
        let over = last_req + 1;
        session
            .send(&crate::v2::to_frame(&OpenStandardMiningChannel {
                request_id: over,
                user_identity: "cap-overflow".to_string(),
                nominal_hash_rate: 0.0,
                max_target: [0xffu8; 32],
            }))
            .expect("send over-cap open");
        let err =
            crate::v2::from_frame::<OpenMiningChannelError>(&session.recv().expect("refusal"))
                .expect("decode refusal");
        assert_eq!(err.request_id, over);
        assert_eq!(err.error_code, "too many channels");

        drop(session);
        server.stop();
    }

    /// Regression (audit finding: open-pool worker names unbounded up to
    /// the 64 KiB line cap). A 129-char user_identity is refused with an
    /// explicit error and the session stays up.
    #[test]
    fn v2_overlong_worker_name_is_refused() {
        use crate::v2::{
            OpenMiningChannelError, OpenStandardMiningChannel, SetupConnection, PROTOCOL_MINING,
        };
        use std::net::TcpStream;
        use std::time::Duration;

        let provider = crate::provider::MockProvider::new([0xffu8; 32], [0x00u8; 32], 1_000_000, 0);
        let mut server = spawn_server(provider, "127.0.0.1:0", None, None, None).expect("spawn");
        let authority = crate::v2_session::generate_authority();
        let v2_addr = server
            .spawn_v2_listener("127.0.0.1:0", authority, Duration::from_secs(3600))
            .expect("v2 listen");
        let stream = TcpStream::connect(v2_addr).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
        let mut session =
            crate::v2_session::V2Session::connect(stream, Some(authority.0)).expect("handshake");
        crate::v2_session::setup_connection(
            &mut session,
            &SetupConnection {
                protocol: PROTOCOL_MINING,
                min_version: 2,
                max_version: 2,
                flags: 0,
                endpoint_host: "x".to_string(),
                endpoint_port: 1,
                vendor: "t".to_string(),
                hardware_version: "cpu".to_string(),
                firmware: "0.2".to_string(),
                device_id: "x".to_string(),
            },
        )
        .expect("negotiate");
        session
            .send(&crate::v2::to_frame(&OpenStandardMiningChannel {
                request_id: 1,
                user_identity: "w".repeat(129),
                nominal_hash_rate: 0.0,
                max_target: [0xffu8; 32],
            }))
            .expect("send overlong open");
        let err =
            crate::v2::from_frame::<OpenMiningChannelError>(&session.recv().expect("refusal"))
                .expect("decode refusal");
        assert_eq!(err.request_id, 1);
        assert_eq!(err.error_code, "worker name too long (max 128 chars)");

        drop(session);
        server.stop();
    }

    #[test]
    fn v2_multi_channel_routing_and_close() {
        use crate::v2::{
            CloseChannel, LuckyJob, LuckySubmit, OpenStandardMiningChannel,
            OpenStandardMiningChannelSuccess, SetupConnection, SubmitSharesError,
            SubmitSharesSuccess, MSG_LUCKY_JOB, MSG_SUBMIT_SHARES_ERROR, MSG_SUBMIT_SHARES_SUCCESS,
            PROTOCOL_MINING,
        };
        use std::net::TcpStream;
        use std::time::Duration;

        // Tight net target: shares accept, never blocks → height static, no
        // rotation jobs interleave.
        let provider = crate::provider::MockProvider::new([0xffu8; 32], [0x00u8; 32], 1_000_000, 0);
        let mut server = spawn_server(provider, "127.0.0.1:0", None, None, None).expect("spawn");
        let authority = crate::v2_session::generate_authority();
        let v2_addr = server
            .spawn_v2_listener("127.0.0.1:0", authority, Duration::from_secs(3600))
            .expect("v2 listen");
        let stream = TcpStream::connect(v2_addr).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
        let mut session =
            crate::v2_session::V2Session::connect(stream, Some(authority.0)).expect("handshake");
        crate::v2_session::setup_connection(
            &mut session,
            &SetupConnection {
                protocol: PROTOCOL_MINING,
                min_version: 2,
                max_version: 2,
                flags: 0,
                endpoint_host: "x".to_string(),
                endpoint_port: 1,
                vendor: "t".to_string(),
                hardware_version: "cpu".to_string(),
                firmware: "0.2".to_string(),
                device_id: "x".to_string(),
            },
        )
        .expect("negotiate");

        // Open two channels (request ids keep the successes distinct).
        for (req, worker) in [(1u32, "mchan-a"), (2, "mchan-b")] {
            session
                .send(&crate::v2::to_frame(&OpenStandardMiningChannel {
                    request_id: req,
                    user_identity: worker.to_string(),
                    nominal_hash_rate: 0.0,
                    max_target: [0xffu8; 32],
                }))
                .expect("open");
        }
        let mut chan_jobs = Vec::new();
        for req in [1u32, 2] {
            let success = crate::v2::from_frame::<OpenStandardMiningChannelSuccess>(
                &session.recv().expect("open success"),
            )
            .expect("decode open");
            assert_eq!(success.request_id, req);
            let job_frame = session.recv().expect("job");
            assert_eq!(job_frame.msg_type, MSG_LUCKY_JOB);
            let job = LuckyJob::from_frame(&job_frame).expect("decode job");
            assert_eq!(job.channel_id, success.channel_id);
            chan_jobs.push((success.channel_id, job));
        }
        assert_ne!(chan_jobs[0].0, chan_jobs[1].0, "channels must be distinct");
        let (chan_a, job_a) = chan_jobs[0].clone();
        let (chan_b, job_b) = chan_jobs[1].clone();

        // One share each (distinct nonces: the header is shared, so the
        // same nonce twice would be one duplicate, not two shares).
        for (ch, job, seq, nonce) in [(chan_a, &job_a, 0u32, 0u64), (chan_b, &job_b, 0u32, 5u64)] {
            session
                .send(&crate::v2::to_frame(&LuckySubmit {
                    channel_id: ch,
                    job_id: job.job_id,
                    sequence_number: seq,
                    nonce,
                }))
                .expect("submit");
            let ack = session.recv().expect("ack");
            assert_eq!(ack.msg_type, MSG_SUBMIT_SHARES_SUCCESS);
            let ok = SubmitSharesSuccess::from_frame(&ack).expect("decode ack");
            assert_eq!(ok.last_sequence_number, seq);
        }
        assert_eq!(server.stats().shares, 2);
        let snap = server.workers(now_secs());
        for worker in ["mchan-a", "mchan-b"] {
            let w = snap.iter().find(|w| w.name == worker).expect("worker");
            assert_eq!(w.shares, 1, "{worker} must own exactly its share");
        }

        // Close A: later submits there name the problem; B is unaffected.
        session
            .send(&crate::v2::to_frame(&CloseChannel {
                channel_id: chan_a,
                reason_code: "done".to_string(),
            }))
            .expect("close");
        session
            .send(&crate::v2::to_frame(&LuckySubmit {
                channel_id: chan_a,
                job_id: job_a.job_id,
                sequence_number: 1,
                nonce: 1,
            }))
            .expect("submit on closed");
        let nack = session.recv().expect("nack");
        assert_eq!(nack.msg_type, MSG_SUBMIT_SHARES_ERROR);
        let err = SubmitSharesError::from_frame(&nack).expect("decode nack");
        assert_eq!(err.error_code, "unknown channel");
        session
            .send(&crate::v2::to_frame(&LuckySubmit {
                channel_id: chan_b,
                job_id: job_b.job_id,
                sequence_number: 1,
                nonce: 6,
            }))
            .expect("submit on B");
        let ack = session.recv().expect("ack B");
        assert_eq!(ack.msg_type, MSG_SUBMIT_SHARES_SUCCESS);
        assert_eq!(server.stats().shares, 3);
        assert_eq!(server.stats().rejects, 0);
        server.stop();
    }
}

#[cfg(test)]
mod division_tests {
    use super::*;
    use crate::provider::MockProvider;

    fn pending(pairs: &[(u64, u64)]) -> BTreeMap<u64, u64> {
        pairs.iter().copied().collect()
    }

    fn credited(heights: &[u64]) -> HashSet<u64> {
        heights.iter().copied().collect()
    }

    #[test]
    fn missing_credits_covers_the_whole_advance() {
        let m = missing_prize_credits(
            100,
            103,
            &pending(&[(101, 10), (102, 20), (103, 30)]),
            &credited(&[]),
        );
        assert_eq!(m, vec![(101, 10), (102, 20), (103, 30)]);
    }

    #[test]
    fn missing_credits_skips_already_credited_heights() {
        let m = missing_prize_credits(
            100,
            103,
            &pending(&[(101, 10), (102, 20)]),
            &credited(&[101]),
        );
        assert_eq!(m, vec![(102, 20)]);
    }

    #[test]
    fn missing_credits_skips_heights_with_no_prospective_record() {
        // Pool was down/behind for 102: never guess, leave it unmarked.
        let m = missing_prize_credits(100, 103, &pending(&[(101, 10), (103, 30)]), &credited(&[]));
        assert_eq!(m, vec![(101, 10), (103, 30)]);
    }

    #[test]
    fn missing_credits_never_goes_backward() {
        let p = pending(&[(99, 10)]);
        assert!(missing_prize_credits(100, 100, &p, &credited(&[])).is_empty());
        assert!(missing_prize_credits(100, 99, &p, &credited(&[])).is_empty());
    }

    #[test]
    fn missing_credits_passes_zero_amounts_through_for_marking() {
        // Zero-overflow heights still get marked done so the range never
        // rescans them (the caller inserts rather than credits).
        let m = missing_prize_credits(100, 101, &pending(&[(101, 0)]), &credited(&[]));
        assert_eq!(m, vec![(101, 0)]);
    }

    #[test]
    fn mock_template_carries_the_prospective_prize_advisory() {
        let mut p = MockProvider::new([0xffu8; 32], [0xffu8; 32], 1_000_000, 0);
        assert_eq!(p.current().prize, 0);
        p.set_prize(876_192);
        let t = p.current();
        assert_eq!((t.height, t.prize), (1, 876_192));
    }

    #[test]
    fn latest_draw_tracks_the_template_advisory() {
        // The refresh loop copies the template's draw advisory into the
        // dashboard status every tick — here via a mock advertising one.
        let mut provider =
            crate::provider::MockProvider::new([0xffu8; 32], [0xffu8; 32], 1_000_000, 0);
        provider.set_draw(14, 200_000_000, 159_200_000, "LtDrawWinner1");
        let server = spawn_server(provider, "127.0.0.1:0", None, None, None).expect("spawn");
        let mut seen = DrawStatus::default();
        for _ in 0..50 {
            seen = server.latest_draw();
            if seen.epoch == 14 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        assert_eq!(seen.epoch, 14);
        assert_eq!(seen.budget, 200_000_000);
        assert_eq!(seen.remaining, 159_200_000);
        assert_eq!(seen.winner, "LtDrawWinner1");
        server.stop();
    }

    /// Prize credits are exposed per height for the explorer, including
    /// heights the pool did not mine (staker/peer finds). Zero-amount
    /// credits (no overflow that height) are not logged.
    #[test]
    fn prize_credits_log_credited_heights_and_survives_restore() {
        let provider = crate::provider::MockProvider::new([0xffu8; 32], [0xffu8; 32], 1_000_000, 0);
        let server = spawn_server(provider, "127.0.0.1:0", None, None, None).expect("spawn");
        assert!(credit_prize_once(&server.shared, 10, 500, "tip"));
        assert!(credit_prize_once(&server.shared, 11, 750, "tip"));
        assert!(credit_prize_once(&server.shared, 12, 0, "tip"));
        // Credit-once: a second attempt for the same height is a no-op.
        assert!(!credit_prize_once(&server.shared, 10, 500, "tip"));
        assert_eq!(server.prize_credits(), vec![(10, 500), (11, 750)]);
        // Restore path (state-file load) replaces the log wholesale.
        server.restore_prize_credits(&[(290, 1_841_270)]);
        assert_eq!(server.prize_credits(), vec![(290, 1_841_270)]);
        server.stop();
    }
    #[test]
    fn extra_listener_shares_state_with_main() {
        // A second V1 bind (e.g. TLS-only :3443 next to plaintext :3346)
        // serves the SAME pool: authorizing through it registers the
        // worker on the shared ledger, and caps still apply pool-wide.
        let provider = crate::provider::MockProvider::new([0xffu8; 32], [0xffu8; 32], 1_000_000, 0);
        let server = spawn_server(provider, "127.0.0.1:0", None, None, None).expect("spawn");
        let extra = server
            .spawn_extra_listener("127.0.0.1:0", None)
            .expect("extra bind");
        assert_ne!(extra, server.addr());
        let mut c = crate::client::PoolClient::connect(extra).expect("connect extra");
        assert!(c.subscribe("probe/0.1").expect("subscribe"));
        assert!(c.authorize("extra-w").expect("authorize"));
        assert!(server
            .names()
            .into_iter()
            .any(|(m, _)| m == "extra-w"));
        server.stop();
    }

    #[test]
    fn v1_overlong_subscribe_agent_is_truncated() {
        // Subscribe agents are arbitrary client strings stored per worker
        // and rendered on the dashboard: cap them like worker names
        // instead of persisting unbounded input.
        let provider = crate::provider::MockProvider::new([0xffu8; 32], [0xffu8; 32], 1_000_000, 0);
        let server = spawn_server(provider, "127.0.0.1:0", None, None, None).expect("spawn");
        let mut c = crate::client::PoolClient::connect(server.addr()).expect("connect");
        assert!(c.subscribe(&"a".repeat(500)).expect("subscribe"));
        assert!(c.authorize("trunc-me").expect("authorize"));
        let agent = server
            .agents()
            .into_iter()
            .find(|(m, _)| m == "trunc-me")
            .map(|(_, a)| a)
            .expect("worker recorded");
        assert_eq!(agent, "a".repeat(MAX_WORKER_NAME_LEN));
        server.stop();
    }
}
