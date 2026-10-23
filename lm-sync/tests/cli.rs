//! CLI contract: `--version` / `--help` exit 0 with `<name> <version>`.

use std::process::Command;

fn run(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_lm-sync"))
        .args(args)
        .output()
        .expect("run lm-sync")
}

#[test]
fn version_and_help() {
    for arg in ["--version", "--help"] {
        let out = Command::new(env!("CARGO_BIN_EXE_lm-sync"))
            .arg(arg)
            .output()
            .expect("run lm-sync");
        assert!(out.status.success(), "{arg}: {}", String::from_utf8_lossy(&out.stderr));
    }
    let out = Command::new(env!("CARGO_BIN_EXE_lm-sync"))
        .arg("--version")
        .output()
        .expect("run --version");
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    assert_eq!(
        stdout.trim_end(),
        format!("lm-sync {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn unknown_flags_and_bad_numbers_exit_2() {
    // Typos used to vanish silently (wrong credentials, ignored flags).
    let cases: &[&[&str]] = &[
        &["pull", "--from", "a", "--to", "b", "--bogus"],
        &["watch", "--from", "a", "--to", "b", "--stall-secs", "abc"],
        &["watch", "--from", "a", "--to", "b", "--interval", "0"],
        &["watch", "--from", "a", "--to", "b", "--stall-secs", "0"],
        &["status", "--node", "a", "--bogus"],
        &["pull"],
        &["frobnicate"],
    ];
    for args in cases {
        let out = run(args);
        assert_eq!(out.status.code(), Some(2), "args={args:?}");
    }
}

#[test]
fn dead_node_is_exit_1_not_silent() {
    // Endpoints still parse; the failure surfaces from the RPC call.
    let out = run(&[
        "pull",
        "--from", "127.0.0.1:1", "--from-user", "u", "--from-pass", "p",
        "--to", "127.0.0.1:1", "--to-user", "u", "--to-pass", "p",
    ]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("lm-sync: error:"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
