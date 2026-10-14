//! Vardiff adaptation over the wire: a fast miner earns harder targets.
//!
//! Mock chain with trivial pool target and an impossible network target:
//! every share is accepted, nothing is ever a block, so job ids change only
//! through vardiff retargets — any new job is adaptation, deterministically.

use lm_pow::{meets_target, pow_hash};
use lm_stratum::client::PoolClient;
use lm_stratum::provider::MockProvider;
use lm_stratum::server::spawn_server;
use lm_stratum::vardiff::target_for_work;
use std::time::Duration;

fn grind(header: &[u8], height: u64, target: &[u8; 32], from: &mut u64) -> u64 {
    for _ in 0..10_000 {
        let n = *from;
        *from += 1;
        if meets_target(&pow_hash(header, n, height), target) {
            return n;
        }
    }
    panic!("no share in 10k nonces");
}

#[test]
fn fast_miner_steps_up_twice() {
    let server = spawn_server(
        MockProvider::new([0xffu8; 32], [0x00u8; 32], 1_000_000, 0),
        "127.0.0.1:0",
        None,
        None,
        None
    )
    .expect("bind");
    let mut c = PoolClient::connect(server.addr()).expect("connect");
    assert!(c.subscribe("vardiff-probe").expect("sub"));
    assert!(c.authorize("speedy").expect("auth"));

    let job1 = c
        .read_job(Duration::from_secs(5))
        .expect("io")
        .expect("job1");
    assert_eq!(job1.pool_target, [0xffu8; 32]);

    // Window 1: 8 instant shares at work 1 → burst pace → 4× → work 4.
    let mut nonce = 0u64;
    for _ in 0..8 {
        let n = grind(&job1.header, job1.height, &job1.pool_target, &mut nonce);
        let (ok, err) = c.submit("speedy", &job1.id, n).expect("submit");
        assert!(ok, "{err:?}");
    }
    let job2 = c
        .read_job(Duration::from_secs(5))
        .expect("io")
        .expect("job2");
    assert_ne!(job2.id, job1.id);
    assert_eq!(job2.pool_target, target_for_work(4));

    // Window 2: 8 shares at work 4 (~4 hashes each) → 4× again → work 16.
    for _ in 0..8 {
        let n = grind(&job2.header, job2.height, &job2.pool_target, &mut nonce);
        let (ok, err) = c.submit("speedy", &job2.id, n).expect("submit");
        assert!(ok, "{err:?}");
    }
    let job3 = c
        .read_job(Duration::from_secs(5))
        .expect("io")
        .expect("job3");
    assert_ne!(job3.id, job2.id);
    assert_eq!(job3.pool_target, target_for_work(16));

    // Ledger credits follow the adapted difficulty: 8×1 + 8×4 = 40.
    assert_eq!(server.stats().shares, 16);
    assert_eq!(server.stats().blocks, 0);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let agg = server.aggregate(30 * 86_400, now);
    assert_eq!(agg.len(), 1);
    assert_eq!(agg[0].raw_work, 40);

    server.stop();
}
