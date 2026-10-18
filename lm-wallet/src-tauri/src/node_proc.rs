//! Wallet-managed node daemon (luckyd): start/stop/status for YOUR node
//! on this box.
//!
//! The wallet never supervises anything (standing rule): it starts the
//! configured binary as a plain foreground child, tracks the PID it
//! started, and stops ONLY that child. If the process dies on its own the
//! slot clears on the next status check. No auto-restart, no watchdogs.

use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

/// Default RPC port when the URL carries none: what a flagless
/// `-testnet` node binds on a clean machine (empirical default,
/// re-verified 2026-10-18 on an empty datadir; a chain conf `rpcport=`
/// overrides it per install, and the wallet's managed node always
/// passes `-rpcport` explicitly anyway).
pub const DEFAULT_RPC_PORT: u16 = 42253;

/// Placeholder credentials shipped in a fresh config (`u`/`p` — the shared
/// testnet node's public pair). When the wallet manages its own node it
/// replaces these with generated ones instead of asking the user.
pub const PLACEHOLDER_USER: &str = "u";
pub const PLACEHOLDER_PASS: &str = "p";

/// A node this app started (and may therefore stop).
pub struct ManagedNode {
    pub child: Child,
    pub pid: u32,
}

/// Port parsed from an rpc_url, or the default when absent/unparseable.
pub fn rpc_port_from_url(url: &str) -> u16 {
    let no_scheme = url.rsplit("://").next().unwrap_or(url);
    let hostport = no_scheme.split('/').next().unwrap_or(no_scheme);
    hostport
        .rsplit(':')
        .next()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(DEFAULT_RPC_PORT)
}

/// Per-OS home directory. `HOME` does not exist on Windows (`USERPROFILE`
/// does) — the old `unwrap_or("/tmp")` fallback silently parked a managed
/// node's datadir in /tmp on Windows.
pub fn home_dir() -> Option<std::path::PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("USERPROFILE").map(std::path::PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME").map(std::path::PathBuf::from)
    }
}

/// Default datadir for a `-testnet` node, mirroring the daemon's own
/// GetDefaultDataDir() for LUCKYLODETEST so an existing chain is found:
/// Unix ~/.luckylodetest, macOS ~/Library/Application Support/LuckyLodeTest,
/// Windows %APPDATA%\LuckyLodeTest (util.cpp — Komodo paths are upstream
/// chains only).
pub fn default_datadir() -> Result<String, String> {
    #[cfg(windows)]
    {
        let appdata = std::env::var("APPDATA")
            .map_err(|_| "no %APPDATA% — set a node datadir in Settings".to_string())?;
        Ok(format!(r"{appdata}\LuckyLodeTest"))
    }
    #[cfg(all(not(windows), target_os = "macos"))]
    {
        let home = home_dir().ok_or_else(|| "no home directory — set a node datadir in Settings".to_string())?;
        Ok(home.join("Library/Application Support/LuckyLodeTest").to_string_lossy().into_owned())
    }
    #[cfg(all(not(windows), not(target_os = "macos")))]
    {
        let home = home_dir().ok_or_else(|| "no home directory — set a node datadir in Settings".to_string())?;
        Ok(home.join(".luckylodetest").to_string_lossy().into_owned())
    }
}

/// Datadir for the managed node: configured, or the per-OS default.
pub fn resolve_datadir(configured: &str) -> Result<String, String> {
    let t = configured.trim();
    if !t.is_empty() {
        return Ok(t.to_string());
    }
    default_datadir()
}

/// Candidate node-binary locations for one run. PATH first (what a CLI
/// install puts in reach), then per-OS install conventions.
fn search_dirs() -> Vec<std::path::PathBuf> {
    let mut dirs: Vec<std::path::PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    if let Some(home) = home_dir() {
        #[cfg(windows)]
        {
            for d in ["verus", r"Applications\verus", r".local\bin"] {
                dirs.push(home.join(d));
            }
        }
        #[cfg(not(windows))]
        {
            for d in ["Applications", "bin", ".local/bin", "verus/src"] {
                dirs.push(home.join(d));
            }
        }
    }
    #[cfg(not(windows))]
    dirs.push(std::path::PathBuf::from("/usr/local/bin"));
    dirs
}

/// Find a runnable node daemon. `luckyd` first everywhere; a legacy
/// `verusd` (pre-rename install) is accepted as a fallback so existing
/// boxes keep working. First hit wins; only files that exist, are
/// regular and (unix) executable are returned. Testable via
/// `find_luckyd_in`.
pub fn find_luckyd() -> Vec<String> {
    let dirs = search_dirs();
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    for name in ["luckyd", "verusd"] {
        let exe = if cfg!(windows) {
            format!("{name}.exe")
        } else {
            name.to_string()
        };
        candidates.extend(dirs.iter().map(|d| d.join(&exe)));
    }
    find_luckyd_in(&candidates)
}

/// `find_luckyd` over explicit candidates (unit-testable, no PATH games).
pub fn find_luckyd_in(candidates: &[std::path::PathBuf]) -> Vec<String> {
    let mut found = Vec::new();
    for c in candidates {
        let meta = match std::fs::metadata(c) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if !meta.is_file() {
            continue;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if meta.permissions().mode() & 0o111 == 0 {
                continue;
            }
        }
        #[allow(clippy::single_char_add_str)]
        found.push(c.to_string_lossy().into_owned());
    }
    found
}

/// Wallet-generated RPC credentials for a node this app starts. The user
/// never sees or types them: the wallet passes them on the node's command
/// line and keeps the matching pair in its own config.
pub fn generate_rpc_creds() -> (String, String) {
    use rand::RngCore;
    let mut raw = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut raw);
    (
        "luckywallet".to_string(),
        raw.iter().map(|b| format!("{b:02x}")).collect(),
    )
}

/// Does this credential pair need replacing before we manage a node?
/// Empty (never set) and the shipped shared-node placeholders both do;
/// anything else is the user's own pair and is left alone.
pub fn needs_generated_creds(user: &str, pass: &str) -> bool {
    let empty = user.trim().is_empty() || pass.trim().is_empty();
    let placeholder = user == PLACEHOLDER_USER && pass == PLACEHOLDER_PASS;
    empty || placeholder
}

/// argv for the managed node (foreground — never -daemon, or the Child
/// handle would track a forked-away double-fork... the daemon's -daemon
/// forks, so we run attached and let it log to its debuglogfile). RPC is
/// pinned to loopback: a wallet-managed node must never expose its RPC
/// port.
pub fn build_args(datadir: &str, user: &str, pass: &str, port: u16, extra: &str) -> Vec<String> {
    let mut args = vec![
        format!("-datadir={datadir}"),
        format!("-rpcuser={user}"),
        format!("-rpcpassword={pass}"),
        format!("-rpcport={port}"),
        "-rpcbind=127.0.0.1".to_string(),
        "-server=1".to_string(),
        "-daemon=0".to_string(),
        format!("-debuglogfile={datadir}/wallet-managed-debug.log"),
    ];
    for tok in extra.split_whitespace() {
        if tok == "-daemon" || tok.starts_with("-daemon=") {
            continue; // foreground only — a daemon fork escapes Child
        }
        args.push(tok.to_string());
    }
    args
}

/// Spawn the managed node. Returns the child on successful spawn (RPC
/// readiness is the caller's `wait_for_rpc`).
pub fn spawn(bin: &str, args: &[String]) -> Result<Child, String> {
    if bin.trim().is_empty() {
        return Err("no node binary configured (Settings → Node → luckyd path)".to_string());
    }
    let meta = std::fs::metadata(bin)
        .map_err(|e| format!("node binary {bin}: {e}"))?;
    if meta.is_dir() {
        return Err(format!("node binary {bin} is a directory"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o111 == 0 {
            return Err(format!("node binary {bin} is not executable"));
        }
    }
    std::process::Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("start {bin}: {e}"))
}

/// Poll getblockcount until the node answers (or the deadline passes).
/// On timeout, tails the managed debug log so bind failures surface.
pub fn wait_for_rpc(
    rpc: &lm_stratum::rpc::RpcClient,
    datadir: &str,
    timeout: Duration,
) -> Result<u64, String> {
    let t0 = Instant::now();
    loop {
        if let Ok(v) = rpc.call("getblockcount", serde_json::json!([])) {
            if let Some(h) = v.as_u64() {
                return Ok(h);
            }
        }
        if t0.elapsed() >= timeout {
            let tail = std::fs::read_to_string(format!("{datadir}/wallet-managed-debug.log"))
                .map(|s| {
                    let lines: Vec<&str> = s.lines().collect();
                    lines.iter().rev().take(5).rev().cloned().collect::<Vec<_>>().join(" | ")
                })
                .unwrap_or_else(|_| "(no debug log)".to_string());
            return Err(format!("node did not answer RPC in time — did another node take the port? log tail: {tail}"));
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rpc_port_parsing() {
        assert_eq!(rpc_port_from_url("http://127.0.0.1:42253"), 42253);
        assert_eq!(rpc_port_from_url("127.0.0.1:9271"), 9271);
        assert_eq!(rpc_port_from_url("https://luckylode.cc/node/"), DEFAULT_RPC_PORT);
        assert_eq!(rpc_port_from_url(""), DEFAULT_RPC_PORT);
    }

    #[test]
    fn datadir_default() {
        let d = resolve_datadir("/tmp/x").expect("explicit");
        assert_eq!(d, "/tmp/x");
        let d = resolve_datadir("  /tmp/y  ").expect("explicit trimmed");
        assert_eq!(d, "/tmp/y");
        let d = resolve_datadir("").expect("default");
        // Mirrors the daemon's own GetDefaultDataDir for LUCKYLODETEST.
        #[cfg(target_os = "macos")]
        assert!(d.ends_with("LuckyLodeTest"), "{d}");
        #[cfg(all(unix, not(target_os = "macos")))]
        assert!(d.ends_with(".luckylodetest"), "{d}");
        #[cfg(windows)]
        assert!(d.to_lowercase().ends_with(r"luckylodetest"), "{d}");
    }

    #[test]
    fn discovery_returns_only_runnable_files() {
        let dir = std::env::temp_dir().join(format!("lk-find-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("tmpdir");
        let good = dir.join("luckyd");
        std::fs::write(&good, b"#!/bin/sh\n").expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&good, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }
        let not_exec = dir.join("luckyd-noexec");
        std::fs::write(&not_exec, b"#!/bin/sh\n").expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&not_exec, std::fs::Permissions::from_mode(0o644))
                .expect("chmod");
        }
        let missing = dir.join("luckyd-gone");
        let candidates = vec![good.clone(), not_exec, missing, dir.clone()];
        let found = find_luckyd_in(&candidates);
        assert_eq!(found, vec![good.to_string_lossy().into_owned()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn luckyd_wins_over_legacy_verusd() {
        let dir = std::env::temp_dir().join(format!("lk-find-legacy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("tmpdir");
        let mut candidates = Vec::new();
        // `find_luckyd` always builds its candidate list luckyd-first (the
        // loop order here mirrors it): a leftover pre-rename binary in the
        // same directory can never win over the renamed one.
        for name in ["luckyd", "verusd"] {
            let p = dir.join(name);
            std::fs::write(&p, b"#!/bin/sh\n").expect("write");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755))
                    .expect("chmod");
            }
            candidates.push(p);
        }
        let found = find_luckyd_in(&candidates);
        assert_eq!(found.len(), 2);
        assert!(found[0].ends_with("luckyd"), "{found:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn creds_generated_only_for_placeholder_or_empty() {
        assert!(needs_generated_creds("u", "p"));
        assert!(needs_generated_creds("", ""));
        assert!(needs_generated_creds("u", ""));
        assert!(!needs_generated_creds("alice", "s3cret"));
        let (u, p) = generate_rpc_creds();
        assert_eq!(u, "luckywallet");
        assert_eq!(p.len(), 32);
        assert!(p.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(generate_rpc_creds().1, p, "pass must be random");
    }

    #[test]
    fn args_force_foreground_server_loopback() {
        let a = build_args("/tmp/d", "u", "p", 1234, "-testnet -daemon -mint");
        assert!(a.contains(&"-server=1".to_string()));
        assert!(a.contains(&"-daemon=0".to_string()));
        assert!(a.contains(&"-rpcbind=127.0.0.1".to_string()));
        assert!(!a.iter().any(|x| x == "-daemon"));
        assert!(a.contains(&"-mint".to_string()));
        assert!(a.contains(&"-rpcport=1234".to_string()));
    }

    /// Full lifecycle against a real luckyd: spawn → RPC up → stop →
    /// reaped. Slow (~30s); run explicitly when the node binary changes.
    #[test]
    #[ignore]
    fn live_managed_regtest_lifecycle() {
        let luckyd = std::env::var("LUCKYD_BIN")
            .or_else(|_| std::env::var("VERUSD_BIN"))
            .unwrap_or_default();
        let luckyd = if !luckyd.is_empty() {
            luckyd
        } else {
            // Default: the renamed binary, with the pre-rename name as a
            // fallback for stale dev trees.
            let d = format!("{}/../../verus/src", env!("CARGO_MANIFEST_DIR"));
            let renamed = format!("{d}/luckyd");
            if std::path::Path::new(&renamed).exists() {
                renamed
            } else {
                format!("{d}/verusd")
            }
        };
        let dir = std::env::temp_dir().join(format!("lk-wmtest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("tmpdir");
        let dir_s = dir.to_str().expect("tmpdir utf8").to_string();
        let port = 45553u16;
        let args = build_args(&dir_s, "u", "p", port, "-regtest");
        let mut child = spawn(&luckyd, &args).expect("spawn luckyd");
        let rpc = lm_stratum::rpc::RpcClient::from_url(
            &format!("http://127.0.0.1:{port}"),
            "u",
            "p",
        )
        .expect("rpc client");
        let h = wait_for_rpc(&rpc, &dir_s, Duration::from_secs(180)).expect("rpc up");
        assert_eq!(h, 0);
        let _ = rpc.call("stop", serde_json::json!([]));
        let t0 = Instant::now();
        loop {
            match child.try_wait().expect("try_wait") {
                Some(_) => break,
                None if t0.elapsed() > Duration::from_secs(60) => {
                    child.kill().ok();
                    child.wait().ok();
                    panic!("managed node would not stop");
                }
                None => std::thread::sleep(Duration::from_millis(500)),
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
