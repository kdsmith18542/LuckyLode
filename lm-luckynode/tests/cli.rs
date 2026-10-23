//! CLI contract: `--version` / `--help` exit 0 with `<name> <version>`.

use std::process::Command;

fn base_args(state_file: &str) -> Vec<String> {
    vec![
        "--node-rpc".into(),
        "127.0.0.1:1".into(),
        "--rpc-user".into(),
        "u".into(),
        "--rpc-pass".into(),
        "p".into(),
        "--name".into(),
        "ln-test".into(),
        "--payout-address".into(),
        "m111111111111111111111111111111111".into(),
        "--state-file".into(),
        state_file.into(),
    ]
}

#[test]
fn version_and_help() {
    for arg in ["--version", "--help"] {
        let out = Command::new(env!("CARGO_BIN_EXE_luckynode"))
            .arg(arg)
            .output()
            .expect("run luckynode");
        assert!(out.status.success(), "{arg}: {}", String::from_utf8_lossy(&out.stderr));
    }
    let out = Command::new(env!("CARGO_BIN_EXE_luckynode"))
        .arg("--version")
        .output()
        .expect("run --version");
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    assert_eq!(
        stdout.trim_end(),
        format!("luckynode {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn corrupt_state_file_refuses_before_network() {
    // A corrupt state file must fail closed: silently dropping a pending
    // fee-paying commitment is how duplicates get minted.
    let dir = std::env::temp_dir().join("luckynode-corrupt-state.json");
    std::fs::write(&dir, "{not json").expect("write");
    let out = Command::new(env!("CARGO_BIN_EXE_luckynode"))
        .args(base_args(dir.to_str().expect("path")))
        .output()
        .expect("run luckynode");
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("corrupt state file"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = std::fs::remove_file(&dir);
}

#[test]
fn unreadable_state_path_refuses() {
    // A directory reads back as EISDIR on every user — same fail-closed
    // path as bad permissions, no root-skip needed.
    let dir = std::env::temp_dir().join("luckynode-unreadable-state");
    let _ = std::fs::create_dir_all(&dir);
    let out = Command::new(env!("CARGO_BIN_EXE_luckynode"))
        .args(base_args(dir.to_str().expect("path")))
        .output()
        .expect("run luckynode");
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("cannot read state file"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = std::fs::remove_dir(&dir);
}

#[test]
fn missing_state_file_reaches_the_chain_guard() {
    // Fresh start passes local load, then fails on the dead RPC — proving
    // load swallows nothing and the guard runs before anything spends.
    let dir = std::env::temp_dir().join("luckynode-absent-state.json");
    let _ = std::fs::remove_file(&dir);
    let out = Command::new(env!("CARGO_BIN_EXE_luckynode"))
        .args(base_args(dir.to_str().expect("path")))
        .output()
        .expect("run luckynode");
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("cannot read node chain"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
