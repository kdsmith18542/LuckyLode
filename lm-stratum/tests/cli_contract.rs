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
