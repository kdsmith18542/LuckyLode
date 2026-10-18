//! Regtest e2e driver: fund a fixed Rust key, then build+sign+broadcast
//! a spend back, all against a throwaway regtest node.
//!
//! ```sh
//! # 1. print the Rust address to fund:
//! cargo run -p lm-keys --example regtest_spend
//! # 2. fund it + mine a block (node side):
//! #    sendtoaddress <addr> 0.005 ; generate 1
//! # 3. spend it back:
//! cargo run -p lm-keys --example regtest_spend -- <rpc> <user> <pass> \
//!     <fund_txid> <fund_vout> <dest_addr> <amount_sats>
//! ```

use lm_keys::address::{base58check_decode, Keypair};
use lm_keys::chain::Network;
use lm_keys::sign::{build_spend, Epoch, SpendUtxo, DEFAULT_FEE_RATE_SAT_PER_KB};
use lm_keys::tx::parse_txid_display;
use lm_stratum::rpc::RpcClient;

const SECRET: [u8; 32] = [0x42; 32];

fn addr_to_p2pkh_script(addr: &str, net: Network) -> Result<Vec<u8>, String> {
    let (ver, payload) = base58check_decode(addr)?;
    if ver != net.p2pkh_version() || payload.len() != 20 {
        return Err(format!("not a {net:?} P2PKH address: {addr}"));
    }
    let mut s = vec![0x76, 0xa9, 0x14];
    s.extend_from_slice(&payload);
    s.extend_from_slice(&[0x88, 0xac]);
    Ok(s)
}

fn main() -> Result<(), String> {
    let net = Network::Regtest;
    let kp = Keypair::from_secret(SECRET).map_err(|e| e.to_string())?;
    let own = kp.p2pkh_address(net);
    let args: Vec<String> = std::env::args().collect();
    if args.len() == 1 {
        println!("{own}");
        return Ok(());
    }
    if args.len() != 8 {
        return Err("usage: regtest_spend <rpc> <user> <pass> <fund_txid> <fund_vout> <dest> <amount_sats>".to_string());
    }
    let rpc = RpcClient::from_url(&args[1], &args[2], &args[3]).map_err(|e| e.to_string())?;
    let vout: u32 = args[5].parse().map_err(|_| "bad vout")?;
    let amount: u64 = args[7].parse().map_err(|_| "bad amount")?;
    // Fetch the funding output (proves it exists + is unspent).
    let txout = rpc
        .call("gettxout", serde_json::json!([args[4], vout]))
        .map_err(|e| format!("gettxout: {e}"))?;
    if txout.is_null() {
        return Err("funding output missing or already spent".to_string());
    }
    let coins: f64 = txout
        .get("value")
        .and_then(|v| v.as_f64())
        .ok_or("gettxout: no value")?;
    let value = (coins * 100_000_000.0).round() as u64;
    let script = hex::decode(
        txout
            .pointer("/scriptPubKey/hex")
            .and_then(|v| v.as_str())
            .ok_or("gettxout: no script")?,
    )
    .map_err(|e| format!("script hex: {e}"))?;
    let utxo = SpendUtxo {
        txid: parse_txid_display(&args[4])?,
        vout,
        value,
        script,
        secret: SECRET,
    };
    let dest_script = addr_to_p2pkh_script(&args[6], net)?;
    let change_script = kp.p2pkh_script(net);
    let spend = build_spend(&[utxo], &dest_script, amount, &change_script, DEFAULT_FEE_RATE_SAT_PER_KB, Epoch::Sprout)?;
    println!("spend {} fee={} change={}", spend.txid, spend.fee, spend.change);
    let txid: serde_json::Value = rpc
        .call("sendrawtransaction", serde_json::json!([spend.hex]))
        .map_err(|e| format!("sendrawtransaction REJECTED: {e}"))?;
    println!("accepted: {txid}");
    if txid.as_str().unwrap_or("") != spend.txid {
        return Err("node returned a different txid".to_string());
    }
    Ok(())
}
