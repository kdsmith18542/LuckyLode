//! luckynode daemon: registers a bonded service node with the pool,
//! heartbeats liveness, and cosigns draw results, v1 test).
//!
//! usage:
//!   luckynode --pool HOST:PORT --address ADDR --key-file PATH
//!             [--payout-key-hex HEX | --payout-key-file PATH]
//!             [--bond-utxo TXID:VOUT]
//!             [--heartbeat-interval SECS] [--register-only] [--once]
//! The payout key secret may also come from LUCKYNODE_PAYOUT_KEY (env).
//! --bond-utxo designates the locked bond collateral (mainnet form);
//! without it the bond is a plain address-balance check.
//!
//! The key file holds 32 raw ed25519 secret bytes (created with mode 0600
//! if missing, from /dev/urandom). The node id is the hex pubkey.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use lm_luckynode::notarize;
use lm_luckynode::register_body;

fn usage() -> ! {
    eprintln!(
        "usage:\n  luckynode --pool HOST:PORT --address ADDR --key-file PATH [--payout-key-hex HEX | --payout-key-file PATH | LUCKYNODE_PAYOUT_KEY env] [--bond-utxo TXID:VOUT] [--heartbeat-interval SECS] [--register-only] [--once]"
    );
    std::process::exit(2);
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Minimal HTTP client (std only): GET or POST a JSON body, return the
/// response body on 200. The pool's --http server is HTTP/1.0-ish with
/// `Connection: close`, so read-to-EOF is the framing.
fn http(pool: &str, method: &str, path: &str, body: Option<&str>) -> Result<String, String> {
    let body = body.unwrap_or("");
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {pool}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let mut s =
        TcpStream::connect(pool).map_err(|e| format!("connect {pool}: {e}"))?;
    s.set_read_timeout(Some(Duration::from_secs(15)))
        .map_err(|e| format!("timeout: {e}"))?;
    s.write_all(req.as_bytes())
        .map_err(|e| format!("write: {e}"))?;
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).map_err(|e| format!("read: {e}"))?;
    let text = String::from_utf8_lossy(&raw);
    let (head, body) = text.split_once("\r\n\r\n").ok_or("malformed http response")?;
    let status = head.lines().next().unwrap_or("");
    if !status.contains(" 200") {
        return Err(format!("{status}: {}", body.chars().take(160).collect::<String>()));
    }
    Ok(body.to_string())
}

fn load_or_gen_key(path: &str) -> Result<[u8; 32], String> {
    if let Ok(data) = std::fs::read(path) {
        if data.len() == 32 {
            let mut arr = [0u8; 32];
            arr.copy_from_slice(&data);
            return Ok(arr);
        }
        return Err(format!("key file {path} is not 32 bytes"));
    }
    let mut secret = [0u8; 32];
    let mut f =
        std::fs::File::open("/dev/urandom").map_err(|e| format!("/dev/urandom: {e}"))?;
    f.read_exact(&mut secret)
        .map_err(|e| format!("urandom read: {e}"))?;
    std::fs::write(path, secret).map_err(|e| format!("write {path}: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    println!("generated new ed25519 identity (saved to {path})");
    Ok(secret)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut pool = String::new();
    let mut address = String::new();
    let mut key_file = String::new();
    let mut payout_key_hex = String::new();
    let mut bond_utxo = String::new();
    let mut interval: u64 = 60;
    let mut register_only = false;
    let mut once = false;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--pool" => {
                i += 1;
                pool = args.get(i).unwrap_or_else(|| usage()).clone();
            }
            "--address" => {
                i += 1;
                address = args.get(i).unwrap_or_else(|| usage()).clone();
            }
            "--key-file" => {
                i += 1;
                key_file = args.get(i).unwrap_or_else(|| usage()).clone();
            }
            "--payout-key-hex" => {
                i += 1;
                payout_key_hex = args.get(i).unwrap_or_else(|| usage()).clone();
            }
            "--payout-key-file" => {
                // Secrets off argv: a 0600 file (or the environment) beats a
                // `ps`-visible command line. One hex secret per line.
                i += 1;
                let path = args.get(i).unwrap_or_else(|| usage()).clone();
                payout_key_hex = std::fs::read_to_string(path.trim())
                    .unwrap_or_else(|e| {
                        eprintln!("luckynode: cannot read payout-key-file: {e}");
                        std::process::exit(2);
                    })
                    .lines()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or("")
                    .trim()
                    .to_string();
            }
            "--bond-utxo" => {
                i += 1;
                bond_utxo = args.get(i).unwrap_or_else(|| usage()).clone();
            }
            "--heartbeat-interval" => {                i += 1;
                interval = args
                    .get(i)
                    .unwrap_or_else(|| usage())
                    .parse()
                    .unwrap_or_else(|_| usage());
            }
            "--register-only" => register_only = true,
            "--once" => once = true,
            _ => usage(),
        }
        i += 1;
    }
    if pool.is_empty() || address.is_empty() || key_file.is_empty() {
        usage();
    }
    // Precedence: --payout-key-hex / --payout-key-file, then the
    // LUCKYNODE_PAYOUT_KEY environment variable.
    if payout_key_hex.is_empty() {
        if let Ok(k) = std::env::var("LUCKYNODE_PAYOUT_KEY") {
            payout_key_hex = k.trim().to_string();
        }
    }

    let secret = match load_or_gen_key(&key_file) {
        Ok(k) => k,
        Err(e) => {
            eprintln!("luckynode: {e}");
            std::process::exit(1);
        }
    };
    let id = {
        use ed25519_dalek::SigningKey;
        notarize::hex(&SigningKey::from_bytes(&secret).verifying_key().to_bytes())
    };
    // The pool proves payout-address ownership at registration: without the
    // address's key there is no Sybil resistance.
    let payout_secret = if payout_key_hex.is_empty() {
        eprintln!(
            "luckynode: this pool requires payout-address ownership proof.\n\
             pass --payout-key-hex <32-byte hex secret of the payout address>, or register from the Lucky Lode wallet."
        );
        std::process::exit(1);
    } else {
        match notarize::unhex(&payout_key_hex) {
            Ok(b) if b.len() == 32 => {
                let mut s = [0u8; 32];
                s.copy_from_slice(&b);
                s
            }
            _ => {
                eprintln!("luckynode: --payout-key-hex must be 64 hex chars (32 bytes)");
                std::process::exit(1);
            }
        }
    };
    println!("luckynode {id} -> pool {pool} (payout {address})");
    // Locked collateral, mainnet form: sent on every registration (first
    // designate and later re-registers alike — the pool keeps it across
    // bare re-registers, but resending survives even a pool state wipe).
    let bond_opt = if bond_utxo.is_empty() {
        None
    } else {
        Some(bond_utxo.as_str())
    };

    let reg = match register_body(&id, &address, &payout_secret, bond_opt) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("luckynode: register sign failed: {e}");
            std::process::exit(1);
        }
    };
    match http(&pool, "POST", "/api/luckynode/register", Some(&reg.to_string())) {
        Ok(b) => println!("register: {b}"),
        Err(e) => {
            eprintln!("luckynode: register failed: {e}");
            std::process::exit(1);
        }
    }
    if register_only {
        return;
    }

    loop {
        let ts = now_secs();
        let msg = notarize::heartbeat_msg(&id, ts);
        let sig = match notarize::sign(&secret, &msg) {
            Ok(s) => notarize::hex(&s),
            Err(e) => {
                eprintln!("luckynode: sign failed: {e}");
                break;
            }
        };
        let hb = serde_json::json!({ "id": id, "timestamp": ts, "sig": sig }).to_string();
        match http(&pool, "POST", "/api/luckynode/heartbeat", Some(&hb)) {
            Ok(b) => println!("heartbeat @{ts}: {b}"),
            Err(e) => {
                eprintln!("luckynode: heartbeat failed: {e}");
                // Pool restarted (registry wiped): re-register, then retry.
                if e.contains("unknown node") {
                    match register_body(&id, &address, &payout_secret, bond_opt) {
                        Ok(reg) => match http(&pool, "POST", "/api/luckynode/register", Some(&reg.to_string())) {
                            Ok(b) => println!("re-register: {b}"),
                            Err(e) => eprintln!("luckynode: re-register failed: {e}"),
                        },
                        Err(e) => eprintln!("luckynode: re-register sign failed: {e}"),
                    }
                }
            }
        }
        // Cosign anything pending (draw results, jackpot/pot events).
        match http(&pool, "GET", "/api/luckynode/pending", None) {
            Ok(b) => {
                let v: serde_json::Value = serde_json::from_str(&b).unwrap_or_default();
                for p in v.as_array().cloned().unwrap_or_default() {
                    let draw_id = p.get("draw_id").and_then(|x| x.as_str()).unwrap_or("");
                    let block_hash = p.get("block_hash").and_then(|x| x.as_str()).unwrap_or("");
                    if draw_id.is_empty() || block_hash.is_empty() {
                        continue;
                    }
                    let cmsg = notarize::cosign_msg(draw_id, block_hash);
                    match notarize::sign(&secret, &cmsg) {
                        Ok(s) => {
                            let body = serde_json::json!({
                                "draw_id": draw_id, "id": id, "sig": notarize::hex(&s),
                            })
                            .to_string();
                            match http(&pool, "POST", "/api/luckynode/notarize", Some(&body)) {
                                Ok(r) => println!("cosigned {draw_id}: {r}"),
                                Err(e) => eprintln!("luckynode: cosign {draw_id} failed: {e}"),
                            }
                        }
                        Err(e) => eprintln!("luckynode: sign failed: {e}"),
                    }
                }
            }
            Err(e) => eprintln!("luckynode: pending fetch failed: {e}"),
        }
        if once {
            return;
        }
        std::thread::sleep(Duration::from_secs(interval.max(5)));
    }
}
