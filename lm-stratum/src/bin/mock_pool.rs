//! One-process mock pool: the reference `lm-stratum` server over
//! [`MockProvider`] — instant blocks, max target, no node required.
//!
//! A dev/QA tool for proving the miner wire end-to-end cheaply: wallets,
//! the Android emulator (point it at `10.0.2.2:<port>`), and smoke tests
//! get shares accepted and blocks "found" without a chain. Not a real
//! pool — never expose it publicly.
//!
//! ```text
//! mock_pool [ADDR]   # default 127.0.0.1:3344
//! ```

use lm_stratum::provider::MockProvider;
use lm_stratum::server::spawn_server;

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("--help") | Some("-h") => {
            println!("mock_pool [ADDR]   # default 127.0.0.1:3344");
            println!("One-process mock pool: instant blocks, max target, no node required.");
            return;
        }
        Some("--version") | Some("-V") => {
            println!("mock_pool {}", env!("CARGO_PKG_VERSION"));
            return;
        }
        _ => {}
    }
    let addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:3344".to_string());
    let server = match spawn_server(MockProvider::instant(1_000_000), &addr, None, None, None) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cannot bind {addr}: {e}");
            std::process::exit(1);
        }
    };
    println!(
        "mock pool on {} (instant blocks, every share wins)",
        server.addr()
    );
    loop {
        std::thread::sleep(std::time::Duration::from_secs(10));
        let s = server.stats();
        println!("shares={} blocks={}", s.shares, s.blocks);
    }
}
