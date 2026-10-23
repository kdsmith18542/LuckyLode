//! CLI end-to-end tests via `CARGO_BIN_EXE_lm-miner`.
//!
//! Kept cheap on purpose: every invocation hashes at most a couple of
//! LuckyHash evaluations (Argon2id ~ms each). These pin the UX contract —
//! flags, exit codes, output shapes — that the solo-mining guide will quote.

use std::process::Command;

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_lm-miner"))
}

fn header_hex() -> String {
    (0u8..80).map(|b| format!("{b:02x}")).collect()
}

#[test]
fn bench_json_reports_one_hash() {
    let out = bin()
        .args(["bench", "--threads", "1", "--hashes", "1", "--json"])
        .output()
        .expect("run bench");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    assert!(stdout.contains("\"total_hashes\":1"), "{stdout}");
    assert!(stdout.contains("\"threads\":1"), "{stdout}");
}

#[test]
fn bench_human_output_shows_rates() {
    let out = bin()
        .args(["bench", "--threads", "1", "--hashes", "1"])
        .output()
        .expect("run bench");
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    assert!(stdout.contains("H/s"), "{stdout}");
    assert!(stdout.contains("thread 0"), "{stdout}");
}

#[test]
fn mine_max_target_finds_nonce_zero() {
    let out = bin()
        .args([
            "mine",
            "--header",
            &header_hex(),
            "--height",
            "1",
            "--target",
            "max",
            "--threads",
            "1",
            "--count",
            "10",
        ])
        .output()
        .expect("run mine");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    assert!(stdout.contains("found nonce=0"), "{stdout}");
    assert!(stdout.contains("ff0d88d9"), "{stdout}");
}

#[test]
fn mine_nbits_pow_limit_parses_and_runs() {
    // 0x1f00ffff is the doc 05 §5.3 POW_LIMIT encoding: parsing must succeed
    // (exit 0 with few nonces almost surely finds nothing -> exit 1 counts as
    // success here; exit 2 would mean the nBits path is broken).
    let out = bin()
        .args([
            "mine",
            "--header",
            &header_hex(),
            "--height",
            "1",
            "--target",
            "nbits:1f00ffff",
            "--threads",
            "1",
            "--count",
            "2",
        ])
        .output()
        .expect("run mine");
    assert_ne!(
        out.status.code(),
        Some(2),
        "nbits parse failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn mine_impossible_target_exhausts_budget_with_exit_1() {
    let out = bin()
        .args([
            "mine",
            "--header",
            &header_hex(),
            "--height",
            "1",
            "--target",
            &"00".repeat(32),
            "--threads",
            "1",
            "--count",
            "2",
        ])
        .output()
        .expect("run mine");
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn usage_errors_exit_2() {
    for args in [vec!["bogus"], vec!["mine", "--header", "zz"], vec![]] {
        let out = bin().args(&args).output().expect("run");
        assert_eq!(out.status.code(), Some(2), "args={args:?}");
    }
}

#[test]
fn version_reports_the_crate_version() {
    let out = bin().arg("--version").output().expect("run --version");
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    assert_eq!(
        stdout.trim_end(),
        format!("lm-miner {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn help_names_the_same_default_agent() {
    // The advertised agent, the help text, and --version must agree —
    // pools label workers by the agent, so a skew is user-visible.
    let out = bin().arg("--help").output().expect("run --help");
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    assert!(
        stdout.contains(&format!("agent=lm-miner/{}", env!("CARGO_PKG_VERSION"))),
        "default agent disagrees with --version"
    );
}
