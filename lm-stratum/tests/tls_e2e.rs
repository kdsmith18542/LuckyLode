//! TLS e2e: a self-signed pool cert, a TLS client that trusts it, and the
//! negative case (a plaintext client must not get through).
//!
//! Certs are generated with the `openssl` CLI (present on the CI image); if
//! openssl is unavailable the test logs and returns rather than failing on
//! unrelated systems.

use lm_stratum::client::PoolClient;
use lm_stratum::provider::MockProvider;
use lm_stratum::server::spawn_server;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

const BASE: u64 = 1_000_000;

/// Generate a test CA and a localhost server cert signed by it. Returns
/// (ca, cert, key).
fn make_cert(dir: &std::path::Path) -> Option<(PathBuf, PathBuf, PathBuf)> {
    let ca_key = dir.join("ca-key.pem");
    let ca = dir.join("ca.pem");
    let key = dir.join("key.pem");
    let csr = dir.join("req.pem");
    let cert = dir.join("cert.pem");
    let run = |args: &[&str]| -> Option<()> {
        let status = Command::new("openssl")
            .args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .ok()?;
        status.success().then_some(())
    };
    run(&[
        "req", "-x509", "-newkey", "rsa:2048", "-nodes",
        "-keyout", ca_key.to_str()?, "-out", ca.to_str()?,
        "-days", "1", "-subj", "/CN=LuckyLode Test CA",
    ])?;
    run(&[
        "req", "-newkey", "rsa:2048", "-nodes",
        "-keyout", key.to_str()?, "-out", csr.to_str()?,
        "-subj", "/CN=localhost",
        "-addext", "subjectAltName=DNS:localhost,IP:127.0.0.1",
    ])?;
    run(&[
        "x509", "-req", "-in", csr.to_str()?, "-CA", ca.to_str()?,
        "-CAkey", ca_key.to_str()?, "-CAcreateserial",
        "-out", cert.to_str()?, "-days", "1",
        "-copy_extensions", "copy",
    ])?;
    Some((ca, cert, key))
}

#[test]
fn tls_pool_round_trip_and_plaintext_rejected() {
    let dir = std::env::temp_dir().join(format!("lm-tls-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    let Some((ca, cert, key)) = make_cert(&dir) else {
        eprintln!("skipping TLS e2e: openssl unavailable");
        return;
    };
    let cfg = lm_stratum::tls::server_config(&cert, &key).expect("server config");

    let server = spawn_server(
        MockProvider::instant(BASE),
        "127.0.0.1:0",
        None,
        None,
        Some(cfg),
    )
    .expect("bind");
    let addr = server.addr();

    // Trusting the self-signed cert as an extra CA must work.
    let mut c = PoolClient::connect_tls(
        addr,
        "localhost",
        Some(&ca),
        false,
        Duration::from_secs(10),
    )
    .expect("tls connect");
    assert!(c.subscribe("tls-probe").expect("subscribe"));
    assert!(c.authorize("tls-worker").expect("authorize"));
    let job = c
        .read_job(Duration::from_secs(5))
        .expect("read")
        .expect("job");
    assert_eq!(job.height, 1);

    // Insecure mode (test-only escape hatch) also connects.
    let mut c2 = PoolClient::connect_tls(addr, "localhost", None, true, Duration::from_secs(10))
        .expect("tls insecure connect");
    assert!(c2.subscribe("tls-insecure").expect("subscribe"));

    // A plaintext client must fail: the pool speaks TLS on this port.
    let mut plain = PoolClient::connect(addr).expect("tcp connect");
    let r = plain.subscribe("plaintext");
    assert!(
        r.is_err(),
        "plaintext client must not be served by a TLS pool, got {r:?}"
    );

    drop(server);
    let _ = std::fs::remove_dir_all(&dir);
}
