//! Pool-mining end-to-end: the compiled binary against a live server.
//!
//! Spawns the reference pool on a mock instant-block chain, then runs
//! `lm-miner pool` as a child process and asserts both sides agree: the
//! child reports accepted shares and a complete session, the server counts
//! the shares and the blocks.

use lm_stratum::provider::MockProvider;
use lm_stratum::server::spawn_server;
use std::process::Command;

#[test]
fn binary_mines_two_shares_against_live_pool() {
    let server = spawn_server(
        MockProvider::instant(1_000_000),
        "127.0.0.1:0",
        None,
        None,
        None,
    )
    .expect("bind");
    let out = Command::new(env!("CARGO_BIN_EXE_lm-miner"))
        .args([
            "pool",
            "--pool",
            &server.addr().to_string(),
            "--worker",
            "e2e-miner",
            "--threads",
            "1",
            "--max-shares",
            "2",
            "--job-timeout",
            "10",
        ])
        .output()
        .expect("run pool miner");
    assert!(
        out.status.success(),
        "exit {:?} stderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    assert!(stdout.contains("share accepted"), "{stdout}");
    assert!(stdout.contains("session complete: 2/2"), "{stdout}");
    // Server side agrees: 2 shares became 2 blocks on the instant chain.
    let stats = server.stats();
    assert_eq!((stats.shares, stats.blocks), (2, 2));
    assert_eq!(server.chain_blocks(), 2);
    server.stop();
}

#[test]
fn pool_connection_and_usage_failures_are_typed() {
    // Missing --worker: usage error.
    let out = Command::new(env!("CARGO_BIN_EXE_lm-miner"))
        .args(["pool", "--pool", "127.0.0.1:1"])
        .output()
        .expect("run");
    assert_eq!(out.status.code(), Some(2));
    // Unparseable pool address: usage error, not a connection attempt.
    let out = Command::new(env!("CARGO_BIN_EXE_lm-miner"))
        .args(["pool", "--pool", "not-an-addr", "--worker", "x"])
        .output()
        .expect("run");
    assert_eq!(out.status.code(), Some(2));
    // Closed port (bound then released): connection failure, exit 1.
    let probe = spawn_server(
        MockProvider::instant(1_000_000),
        "127.0.0.1:0",
        None,
        None,
        None,
    )
    .expect("bind");
    let addr = probe.addr().to_string();
    probe.stop();
    let out = Command::new(env!("CARGO_BIN_EXE_lm-miner"))
        .args([
            "pool",
            "--pool",
            &addr,
            "--worker",
            "x",
            "--job-timeout",
            "2",
            "--fail-fast",
        ])
        .output()
        .expect("run");
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn binary_fails_over_to_fallback_pool() {
    // Primary is a closed port; the fallback is a live mock pool. The
    // miner must land its shares on the fallback without manual help.
    let server = spawn_server(
        MockProvider::instant(1_000_000),
        "127.0.0.1:0",
        None,
        None,
        None,
    )
    .expect("bind");
    let live = server.addr().to_string();
    let dead = {
        let probe = spawn_server(
            MockProvider::instant(1_000_000),
            "127.0.0.1:0",
            None,
            None,
            None,
        )
        .expect("bind");
        let addr = probe.addr().to_string();
        probe.stop();
        addr
    };
    let out = Command::new(env!("CARGO_BIN_EXE_lm-miner"))
        .args([
            "pool",
            "--pool",
            &dead,
            "--fallback-pool",
            &live,
            "--worker",
            "e2e-failover",
            "--threads",
            "1",
            "--max-shares",
            "2",
            "--job-timeout",
            "10",
        ])
        .output()
        .expect("run pool miner");
    assert!(
        out.status.success(),
        "exit {:?} stderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    assert!(stdout.contains("share accepted"), "{stdout}");
    // The Connected line names the endpoint actually served — the
    // fallback, not the dead primary.
    assert!(stdout.contains(&live), "{stdout}");
    let stats = server.stats();
    assert_eq!((stats.shares, stats.blocks), (2, 2));
    server.stop();
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn binary_mines_two_shares_over_v2() {
    // Parity gate for the single session loop: the same two-share session
    // over the V2 (Noise + Lucky jobs) wire must accept and land blocks.
    let mut server = spawn_server(
        MockProvider::instant(1_000_000),
        "127.0.0.1:0",
        None,
        None,
        None,
    )
    .expect("bind");
    let authority = lm_stratum::v2_session::generate_authority();
    let v2 = server
        .spawn_v2_listener(
            "127.0.0.1:0",
            authority,
            std::time::Duration::from_secs(3600),
        )
        .expect("v2 listen");
    let out = Command::new(env!("CARGO_BIN_EXE_lm-miner"))
        .args([
            "pool",
            "--pool",
            &v2.to_string(),
            "--worker",
            "e2e-v2",
            "--threads",
            "1",
            "--max-shares",
            "2",
            "--job-timeout",
            "10",
            "--v2",
            "--v2-authority",
            &hex(&authority.0),
        ])
        .output()
        .expect("run pool miner (v2)");
    assert!(
        out.status.success(),
        "exit {:?} stderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    assert!(stdout.contains("share accepted"), "{stdout}");
    assert!(stdout.contains("session complete: 2/2"), "{stdout}");
    assert_eq!(server.stats().shares, 2);
    server.stop();
}
