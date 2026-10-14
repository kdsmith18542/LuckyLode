//! Background template refresh (finding D): connection threads must never
//! call `provider.current()` — a slow node RPC used to serialize every
//! miner behind the pool's provider lock.
//!
//! The guard is deterministic rather than timing-based: the provider parks
//! the first background refresh (and every later call) inside `current()`.
//! With the fix, connection threads read the broadcast and never touch the
//! provider, so subscribe / authorize / job / submit all round-trip while
//! the "node RPC" hangs forever. With the old inline poll, the connection
//! thread parks too and the client times out.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use lm_stratum::client::PoolClient;
use lm_stratum::provider::{BlockOutcome, JobTemplate, TemplateProvider};
use lm_stratum::server::spawn_server;

/// How long a client waits for a notify, and the per-RPC deadline used by
/// these tests' patients. Generous on purpose: under workspace-wide CPU
/// contention (this box also mines) a connection thread can be starved
/// for seconds, and the default 10 s deadline turns that into a flake.
/// Normal-case latency is well under a second.
const JOB_WAIT: Duration = Duration::from_secs(30);
const RPC_WAIT: Duration = Duration::from_secs(30);

/// Shared call counter and the "hold the provider" gate, for readability.
type Calls = Arc<AtomicUsize>;
type Gate = Arc<(Mutex<bool>, Condvar)>;

/// Provider that counts `current()` calls and parks call #`park_from` and
/// every call after it inside the provider until the gate is released —
/// a node RPC that never returns. The net target rejects every share, so
/// the submit path never needs the provider for a block.
struct GatedProvider {
    calls: Calls,
    gate: Gate,
    park_from: usize,
}

impl TemplateProvider for GatedProvider {
    fn current(&self) -> JobTemplate {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if n >= self.park_from {
            let (lock, cvar) = &*self.gate;
            let mut open = lock.lock().expect("gate lock");
            while !*open {
                open = cvar.wait(open).expect("gate wait");
            }
        }
        JobTemplate {
            header: vec![0xaa; 80],
            height: 1,
            pool_target: [0xffu8; 32],
            net_target: [0x00u8; 32],
        }
    }
    fn submit_block(&mut self, _header: &[u8], _height: u64, _nonce: u64) -> BlockOutcome {
        BlockOutcome {
            accepted: false,
            out: [0u8; 32],
            jackpot: false,
            bonus: 0,
            pot: 0,
            league_overflow: 0,
            reserve: 0,
            coinbase: 0,
        }
    }
}

impl GatedProvider {
    fn new(park_from: usize) -> (Self, Calls, Gate) {
        let calls = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        (
            Self {
                calls: Arc::clone(&calls),
                gate: Arc::clone(&gate),
                park_from,
            },
            calls,
            gate,
        )
    }
}

#[test]
fn connection_threads_never_poll_the_provider() {
    // Call #0 seeds the broadcast inside spawn_server; call #1 is the
    // background refresh, parked inside the provider forever — and so is
    // every later call. A miner must still get served.
    let (provider, calls, gate) = GatedProvider::new(1);
    let server = spawn_server(provider, "127.0.0.1:0", None, None, None).expect("bind");

    let mut c = PoolClient::connect_patient(server.addr(), RPC_WAIT).expect("connect");
    assert!(c.subscribe("slow-node").expect("subscribe"));
    assert!(c.authorize("patient").expect("authorize"));
    let job = c.read_job(JOB_WAIT).expect("io").expect("job");
    assert_eq!(job.id, "00000000");

    // Every submit round-trips while the node RPC hangs: the connection
    // path reads the broadcast, never the provider. The old inline poll
    // parked the connection thread here and the client timed out.
    for nonce in 1..=5u64 {
        let (ok, err) = c
            .submit("patient", &job.id, nonce)
            .expect("submit answered");
        assert!(ok, "submit stalled behind the refresh: {err:?}");
    }
    assert_eq!(server.stats().shares, 5);
    assert_eq!(server.stats().rejects, 0);

    // The counting proof: exactly the spawn seed and the one parked
    // refresh ever called the provider — no connection thread polled it.
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "a connection thread polled the provider (the inline-poll regression)"
    );

    // Release the parked refresh so the thread can exit on shutdown.
    let (lock, cvar) = &*gate;
    *lock.lock().expect("gate lock") = true;
    cvar.notify_all();
    server.stop();
}

#[test]
fn background_refresh_publishes_new_templates() {
    // The other half of the fix: the background thread still notices a
    // chain that moves. A provider whose template advances between polls
    // must reach subscribed miners as a new job with `clean` set.
    struct Advancing {
        calls: Calls,
    }
    impl TemplateProvider for Advancing {
        fn current(&self) -> JobTemplate {
            // Every 5th call returns a fresh height (a new block).
            let n = self.calls.fetch_add(1, Ordering::SeqCst) as u64;
            JobTemplate {
                header: vec![0xaa; 80],
                height: 1 + n / 5,
                pool_target: [0xffu8; 32],
                net_target: [0x00u8; 32],
            }
        }
        fn submit_block(&mut self, _h: &[u8], _ht: u64, _n: u64) -> BlockOutcome {
            BlockOutcome {
                accepted: false,
                out: [0u8; 32],
                jackpot: false,
                bonus: 0,
                pot: 0,
                league_overflow: 0,
                reserve: 0,
                coinbase: 0,
            }
        }
    }

    let calls = Arc::new(AtomicUsize::new(0));
    let server = spawn_server(
        Advancing {
            calls: Arc::clone(&calls),
        },
        "127.0.0.1:0",
        None,
        None,
        None
    )
    .expect("bind");
    let mut c = PoolClient::connect_patient(server.addr(), RPC_WAIT).expect("connect");
    assert!(c.subscribe("watcher").expect("subscribe"));
    assert!(c.authorize("watcher").expect("authorize"));

    // The first job rides the seeded broadcast; a later job only exists if
    // the background refresh polls and publishes (finding D's positive
    // half — refresh still works off the connection path). The provider's
    // height only ever increases, so successive notifies carry strictly
    // higher heights (no fixed seed height is asserted: the refresh may
    // already have advanced past it during setup).
    let first = c.read_job(JOB_WAIT).expect("io").expect("job 1");
    let second = c.read_job(JOB_WAIT).expect("io").expect("job 2");
    assert_ne!(second.id, first.id, "a new template must mint a new job id");
    assert!(
        second.height > first.height,
        "the second job must ride the advanced template ({} -> {})",
        first.height,
        second.height
    );

    server.stop();
}
