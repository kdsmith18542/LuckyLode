//! Testnet-in-a-box: real TCP, real hashes, real money rules.
//!
//! Spins up the reference pool on a mock instant-block chain, drives it with
//! the blocking client, and asserts the whole path: subscribe → authorize →
//! job → grind → submit → share → block → reserve levy → jackpot split →
//! ledger + reconciliation. No mocks past the chain itself.

use lm_pow::{is_jackpot, meets_target, pow_hash};
use lm_stratum::client::PoolClient;
use lm_stratum::provider::MockProvider;
use lm_stratum::server::spawn_server;
use primitive_types::U256;
use std::time::Duration;

const BASE: u64 = 1_000_000;
const LEVY: u64 = 50_000; // 5% of BASE

/// The submit ack is sent BEFORE the pool's block-submit call (a slow
/// node must not push the ack past the miner's deadline), so chain-side
/// effects are eventually visible rather than ack-synchronized: poll.
fn wait_for<F: Fn() -> bool>(what: &str, f: F) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("timed out waiting for {what}");
}

fn grind_share(header: &[u8], height: u64, target: &[u8; 32]) -> (u64, [u8; 32]) {
    for n in 0..10_000u64 {
        let out = pow_hash(header, n, height);
        if meets_target(&out, target) {
            return (n, out);
        }
    }
    panic!("no share in 10k nonces against pool target");
}

fn grind_jackpot(header: &[u8], height: u64, net: &[u8; 32]) -> u64 {
    let t = U256::from_big_endian(net);
    for n in 0..10_000u64 {
        if is_jackpot(&pow_hash(header, n, height), t) {
            return n;
        }
    }
    panic!("no jackpot nonce in 10k tries at 1% odds (p ≈ 0)");
}

#[test]
fn full_loop_share_block_jackpot_and_money() {
    let server = spawn_server(MockProvider::instant(BASE), "127.0.0.1:0", None, None, None).expect("bind");
    let mut c = PoolClient::connect(server.addr()).expect("connect");
    assert!(c.subscribe("lm-miner/0.1").expect("subscribe"));
    assert!(c.authorize("alice").expect("authorize"));
    let alice = "alice".to_string();

    // First job + first share (also the first block on the instant chain).
    let job = c
        .read_job(Duration::from_secs(5))
        .expect("io")
        .expect("job");
    assert_eq!(job.height, 1);
    let (n, _) = grind_share(&job.header, job.height, &job.pool_target);
    let (ok, err) = c.submit("alice", &job.id, n).expect("submit");
    assert!(ok, "first share rejected: {err:?}");
    wait_for("first block to land", || server.chain_blocks() == 1);
    wait_for("chain height 2", || server.chain_height() == 2);

    // Immediate replay is a duplicate, not a second share.
    let (ok, err) = c.submit("alice", &job.id, n).expect("resubmit");
    assert!(!ok);
    assert_eq!(err.map(|(code, _)| code), Some(21));
    assert_eq!(server.stats().shares, 1);

    // Unknown job id is rejected distinctly from stale/dupes.
    let (ok, err) = c.submit("alice", "deadbeef", 0).expect("submit");
    assert!(!ok);
    assert_eq!(err.map(|(code, _)| code), Some(23));

    // Fresh job at the new height; grind a real jackpot nonce for it.
    let job2 = c
        .read_job(Duration::from_secs(5))
        .expect("io")
        .expect("job2");
    assert_eq!(job2.height, 2);
    let jn = grind_jackpot(&job2.header, job2.height, &job2.net_target);
    let (ok, err) = c.submit("alice", &job2.id, jn).expect("jackpot submit");
    assert!(ok, "jackpot share rejected: {err:?}");

    // The block record carries a real bonus split over the PPLNS window
    // (the jackpot block lands after the ack — poll for it).
    wait_for("jackpot block to land", || server.blocks().len() == 2);
    let blocks = server.blocks();
    assert_eq!(blocks.len(), 2);
    let last = blocks.last().expect("jackpot record");
    assert_eq!(last.finder, alice);
    assert!(last.bonus > 0, "reserve held prior levies — bonus must pay");
    assert_eq!(last.payouts.iter().map(|(_, p)| p).sum::<u64>(), last.bonus);

    // Whole-pool money reconciliation: every levied unit is bonus paid,
    // reserve held, prize inflow, or (on the mock's full pot) burned —
    // nothing created or destroyed. The 4/1 levy split sends a fifth of
    // each levy to the pot leg; the mock chain runs the pot at "full"
    // (u64::MAX), so that fifth burns, exactly as the real rules would
    // burn it at a capped pot.
    let bonus_total: u64 = blocks.iter().map(|b| b.bonus).sum();
    let levied = server.chain_blocks() * LEVY;
    let burned = server.chain_blocks() * (LEVY / 5);
    assert_eq!(
        levied,
        bonus_total + server.reserve() + server.prize_balance() + burned
    );

    // The ledger saw alice's work and ranks her.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let agg = server.aggregate(30 * 86_400, now);
    assert_eq!(agg.len(), 1);
    assert_eq!(agg[0].miner, alice);
    assert!(agg[0].raw_work >= 2);

    server.stop();
}

#[test]
fn share_replay_across_connections_is_rejected() {
    // Tight network target: shares are never blocks, so the template (and
    // therefore the shared job id and header) is stable across both
    // connections — the replay rides the identical work identity.
    let server = spawn_server(
        MockProvider::new([0xffu8; 32], [0x00u8; 32], BASE, 0),
        "127.0.0.1:0",
        None,
        None,
        None
    )
    .expect("bind");
    let mut a = PoolClient::connect(server.addr()).expect("connect a");
    let mut b = PoolClient::connect(server.addr()).expect("connect b");
    assert!(a.subscribe("replay").expect("sub a"));
    assert!(b.subscribe("replay").expect("sub b"));
    assert!(a.authorize("alice").expect("auth a"));
    assert!(b.authorize("bob").expect("auth b"));
    let alice = "alice".to_string();

    let job_a = a
        .read_job(Duration::from_secs(5))
        .expect("io")
        .expect("job a");
    // Max pool target accepts any nonce; the zero net target rejects every
    // share, so nothing is a block and the template never rotates.
    let n = 0u64;

    // alice's submit on connection A is accepted and credited once.
    let (ok, err) = a.submit("alice", &job_a.id, n).expect("submit");
    assert!(ok, "first share rejected: {err:?}");
    assert_eq!(server.stats().shares, 1);

    // bob replays the same job id + nonce on a FRESH connection (his job
    // registry started empty, so the per-connection check alone would
    // pass it): the pool-wide work-identity dedupe rejects it.
    let (ok, err) = b.submit("bob", &job_a.id, n).expect("replay");
    assert!(!ok);
    assert_eq!(err.map(|(code, _)| code), Some(21));
    assert_eq!(server.stats().shares, 1, "replay must not credit twice");
    assert_eq!(server.stats().rejects, 1);

    // The ledger saw exactly alice's one share — bob's replay paid nobody.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let agg = server.aggregate(30 * 86_400, now);
    assert_eq!(agg.len(), 1);
    assert_eq!(agg[0].miner, alice);
    assert_eq!(agg[0].raw_work, 1);

    // A different nonce still validates on the replaying connection.
    let (ok, _) = b.submit("bob", &job_a.id, 1).expect("fresh share");
    assert!(ok);
    assert_eq!(server.stats().shares, 2);

    server.stop();
}

#[test]
fn blocks_credit_payout_balances() {
    let server = spawn_server(MockProvider::instant(BASE), "127.0.0.1:0", None, None, None).expect("bind");
    let mut c = PoolClient::connect(server.addr()).expect("connect");
    assert!(c.subscribe("payout-e2e").expect("sub"));
    assert!(c.authorize("dave").expect("auth"));
    let job = c
        .read_job(Duration::from_secs(5))
        .expect("io")
        .expect("job");
    let (n, _) = grind_share(&job.header, job.height, &job.pool_target);
    let (ok, err) = c.submit("dave", &job.id, n).expect("submit");
    assert!(ok, "share rejected: {err:?}");
    // The instant chain made the share a block: payout accounting must
    // total exactly the block value (net value + claimed split). The
    // block lands after the ack (see wait_for) — poll for it.
    wait_for("block to land", || !server.blocks().is_empty());
    let b = server.blocks();
    assert_eq!(b.len(), 1);
    let bals = server.balances();
    assert_eq!(bals.len(), 1);
    assert_eq!(bals[0].0, "dave");
    assert_eq!(
        bals.iter().map(|(_, v)| v).sum::<u64>(),
        b[0].value + b[0].bonus
    );
    // Conservation identity: the lifetime credited coinbase (normal
    // value + jackpot + pot) must equal owed + paid + fees exactly.
    // Prize inflow and seed-fund outputs never touch the miner
    // ledger, so they are not part of either side.
    let (owed, paid, fees) = server.payout_totals();
    assert_eq!(server.credited_total(), owed + paid + fees);
    assert_eq!(
        server.credited_total(),
        b.iter().map(|x| x.value + x.bonus + x.pot).sum::<u64>()
    );

    server.stop();
}

#[test]
fn protocol_edges_are_typed() {
    let server = spawn_server(MockProvider::instant(BASE), "127.0.0.1:0", None, None, None).expect("bind");
    let mut c = PoolClient::connect(server.addr()).expect("connect");

    // Garbage line → JSON-RPC parse error, connection survives.
    let m = c.raw("{not json").expect("raw");
    let err = m.error.expect("error tuple");
    assert_eq!(err.as_array().expect("array")[0], -32700);
    assert!(c.subscribe("probe").expect("subscribe after garbage"));

    // Unknown method → -32601.
    let m = c
        .raw(r#"{"id":9,"method":"mining.nope","params":[]}"#)
        .expect("raw");
    assert_eq!(m.error.expect("e").as_array().expect("a")[0], -32601);

    // Submit before authorize → rejected, not recorded.
    let m = c
        .raw(r#"{"id":10,"method":"mining.submit","params":["bob","j","0000000000000000"]}"#)
        .expect("raw");
    assert_eq!(m.error.expect("e").as_array().expect("a")[0], -32600);
    assert_eq!(server.stats().shares, 0);

    server.stop();
}

#[test]
fn per_worker_tokens_are_enforced() {
    let mut tokens = std::collections::HashMap::new();
    tokens.insert("alice".to_string(), "tok-alice".to_string());
    let server = spawn_server(
        MockProvider::instant(BASE),
        "127.0.0.1:0",
        None,
        Some(tokens),
        None,
    )
    .expect("bind");

    // Correct token authorizes.
    let mut c = PoolClient::connect(server.addr()).expect("connect");
    assert!(c.subscribe("lm-miner/0.2").expect("subscribe"));
    assert!(c.authorize_with("alice", "tok-alice").expect("authorize"));

    // Wrong token is refused.
    let mut c2 = PoolClient::connect(server.addr()).expect("connect");
    assert!(c2.subscribe("lm-miner/0.2").expect("subscribe"));
    assert!(!c2.authorize_with("alice", "wrong").expect("authorize"));

    // Unknown worker is refused even with a valid token for someone else.
    let mut c3 = PoolClient::connect(server.addr()).expect("connect");
    assert!(c3.subscribe("lm-miner/0.2").expect("subscribe"));
    assert!(!c3.authorize_with("mallory", "tok-alice").expect("authorize"));
}
