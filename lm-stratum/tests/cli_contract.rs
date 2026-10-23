//! CLI contract tests: `--version` / `--help` for every `lm-stratum`
//! binary. One convention (`<name> <crate version>` on stdout, exit 0)
//! so operators and scripts can rely on it instead of guessing.

use std::process::Command;

fn run(bin: &str, arg: &str) -> std::process::Output {
    let exe = match bin {
        "pool_server" => env!("CARGO_BIN_EXE_pool_server"),
        "faucet" => env!("CARGO_BIN_EXE_faucet"),
        "monitor" => env!("CARGO_BIN_EXE_monitor"),
        "mock_pool" => env!("CARGO_BIN_EXE_mock_pool"),
        _ => unreachable!(),
    };
    Command::new(exe).arg(arg).output().expect("run binary")
}

fn check_version(bin: &str) {
    let out = run(bin, "--version");
    assert!(out.status.success(), "{bin}: stderr: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    assert_eq!(
        stdout.trim_end(),
        format!("{bin} {}", env!("CARGO_PKG_VERSION")),
        "{bin}: version output"
    );
}

fn check_help(bin: &str) {
    let out = run(bin, "--help");
    assert!(out.status.success(), "{bin}: stderr: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    assert!(stdout.contains("usage") || stdout.contains("mock_pool"), "{bin}: help text");
}

#[test]
fn pool_server_version_and_help() {
    check_version("pool_server");
    check_help("pool_server");
}

#[test]
fn faucet_version_and_help() {
    check_version("faucet");
    check_help("faucet");
}

#[test]
fn monitor_version_and_help() {
    check_version("monitor");
    check_help("monitor");
}

#[test]
fn mock_pool_version_and_help() {
    check_version("mock_pool");
    check_help("mock_pool");
}

#[test]
fn monitor_once_fails_when_the_node_is_down() {
    // --once reports the single poll honestly: unreachable node = exit 1
    // with the alert on stderr (stdout stays clean for plumbing).
    let out = Command::new(env!("CARGO_BIN_EXE_monitor"))
        .args(["--node-rpc", "127.0.0.1:1", "--once"])
        .output()
        .expect("run monitor");
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stdout).is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).contains("CRITICAL node unreachable"));
}

#[test]
fn monitor_rejects_a_zero_interval() {
    for flag in ["--interval", "--stall-secs"] {
        let out = Command::new(env!("CARGO_BIN_EXE_monitor"))
            .args(["--node-rpc", "127.0.0.1:1", flag, "0"])
            .output()
            .expect("run monitor");
        assert_eq!(out.status.code(), Some(2), "{flag}");
    }
}

#[test]
fn pool_server_needs_node_or_mock() {
    // Bare flags used to boot the worthless mock chain silently.
    let out = Command::new(env!("CARGO_BIN_EXE_pool_server"))
        .output()
        .expect("run pool_server");
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("no --node-rpc"), "{stderr}");
}

#[test]
fn pool_server_rejects_over_100_percent_fees() {
    // Past 10000 the fee pot exceeds the block: distributable hits 0
    // and the dev slice overpays from the node wallet. Reject loudly.
    for flag in ["--pool-fee-pp", "--dev-fee-pp"] {
        let out = Command::new(env!("CARGO_BIN_EXE_pool_server"))
            .args([flag, "20000", "--mock"])
            .output()
            .expect("run pool_server");
        assert_eq!(out.status.code(), Some(2), "{flag}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("expects 0-10000"), "{flag}: {stderr}");
    }
}

#[test]
fn pool_server_rejects_bad_bind_fast() {
    // Parse errors exit before any listen: quick, no ports needed.
    let out = Command::new(env!("CARGO_BIN_EXE_pool_server"))
        .args(["--mock", "--bind", "notahostport"])
        .output()
        .expect("run pool_server");
    assert_eq!(out.status.code(), Some(2));
}
