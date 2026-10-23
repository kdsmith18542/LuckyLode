//! CLI contract: `--version` / `--help` exit 0 with `<name> <version>`.

use std::process::Command;

#[test]
fn version_and_help() {
    for arg in ["--version", "--help"] {
        let out = Command::new(env!("CARGO_BIN_EXE_explorer"))
            .arg(arg)
            .output()
            .expect("run explorer");
        assert!(out.status.success(), "{arg}: {}", String::from_utf8_lossy(&out.stderr));
    }
    let out = Command::new(env!("CARGO_BIN_EXE_explorer"))
        .arg("--version")
        .output()
        .expect("run --version");
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    assert_eq!(
        stdout.trim_end(),
        format!("explorer {}", env!("CARGO_PKG_VERSION"))
    );
}
