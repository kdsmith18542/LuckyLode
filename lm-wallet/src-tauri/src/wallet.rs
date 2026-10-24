//! Self-custody wallet backend v2).
//!
//! The app holds its own BIP39 seed (0600 file in the app dir) and derives
//! keys locally (BIP44 `m/44'/coin'/0'/{0,1}/i`, gap limit 20). The node
//! is only a data source (chain height + address index) and a broadcast
//! relay: private keys never leave the machine, and the app only ever
//! shows its own addresses. Every function here is Tauri-free so the same
//! code paths are unit-testable and drivable against a regtest node.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::{aead::Aead, ChaCha20Poly1305, Key, KeyInit, Nonce};
use lm_keys::chain::Network;
use lm_keys::hd;
use lm_keys::mnemonic::SeedPhrase;
use lm_keys::sign;
use lm_keys::tx;
use lm_stratum::rpc::RpcClient;
use rand::{rngs::OsRng, RngCore};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// BIP44 gap limit: scan this many unused addresses past the cursor.
pub const GAP: u32 = 20;
/// Coinbase maturity (consensus): outputs younger than this stay immature.
pub const COINBASE_MATURITY: u64 = 100;
/// Idle auto-lock: money commands lock the wallet after this long idle.
pub const IDLE_LOCK_SECS: u64 = 15 * 60;
const SEED_FILE: &str = "wallet.seed";
const ENC_FILE: &str = "wallet.enc";
const SALT_LEN: usize = 32;
const NONCE_LEN: usize = 12;

fn seed_path(dir: &Path) -> PathBuf {
    dir.join(SEED_FILE)
}

fn enc_path(dir: &Path) -> PathBuf {
    dir.join(ENC_FILE)
}

fn write_0600(path: &Path, data: &[u8]) -> Result<(), String> {
    std::fs::write(path, data).map_err(|e| format!("write {}: {e}", path.display()))?;
    #[cfg(unix)]
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o600))
        .map_err(|e| format!("chmod 0600 {}: {e}", path.display()))?;
    Ok(())
}

// ---- keystore ------------------------------------------------------------

/// Any wallet material present (plaintext legacy or encrypted vault).
pub fn has_any_wallet(dir: &Path) -> bool {
    seed_path(dir).is_file() || enc_path(dir).is_file()
}

/// Read the phrase back (backup display only — the UI must warn loudly).
pub fn reveal_phrase(dir: &Path) -> Result<String, String> {
    let s = std::fs::read_to_string(seed_path(dir)).map_err(|_| "no wallet yet".to_string())?;
    let s = s.trim().to_string();
    SeedPhrase::parse(&s)?; // refuse to show a corrupt file as valid
    Ok(s)
}

// ---- encrypted keystore (W3: lock on open) --------------------------------
//
// The seed file used to be plaintext. Now the phrase is sealed with a key
// derived from the user's spending password (Argon2id) under ChaCha20-Poly1305:
//   wallet.enc = b"LKW1" || salt(32) || nonce(12) || ciphertext
// The app keeps the decrypted seed only in the session (RAM), wiped on lock.
// Old plaintext `wallet.seed` files upgrade exactly once via migrate().

/// Envelope magic (versioned for future KDF upgrades).
const ENC_MAGIC: &[u8; 4] = b"LKW1";
/// Argon2id work factor: interactive login (~0.5 s on a desktop).
const ARGON_M_KIB: u32 = 64 * 1024;
const ARGON_T: u32 = 3;
const ARGON_P: u32 = 1;

/// Unlocked session material. Lives in AppState only; wiped on lock.
pub struct Session {
    pub seed: [u8; 64],
    pub phrase: String,
    pub last_active: Instant,
}

impl Session {
    pub fn new(seed: [u8; 64], phrase: String) -> Self {
        Self {
            seed,
            phrase,
            last_active: Instant::now(),
        }
    }

    pub fn expired(&self) -> bool {
        self.last_active.elapsed().as_secs() > IDLE_LOCK_SECS
    }

    /// Best-effort RAM wipe (not a security boundary against cold-boot,
    /// just hygiene against core dumps and swap).
    pub fn wipe(&mut self) {
        wipe_bytes(&mut self.seed);
        wipe_str(&mut self.phrase);
    }
}

pub fn wipe_bytes(b: &mut [u8]) {
    for x in b.iter_mut() {
        // Volatile write so the optimizer cannot elide the wipe.
        unsafe { std::ptr::write_volatile(x, 0) };
    }
}

fn wipe_str(s: &mut String) {
    // Grows capacity only; length content is what matters.
    unsafe {
        let b = s.as_bytes_mut();
        for x in b.iter_mut() {
            std::ptr::write_volatile(x, 0);
        }
    }
    s.clear();
}

fn check_password(password: &str) -> Result<(), String> {
    if password.len() < 8 {
        return Err("password must be at least 8 characters".to_string());
    }
    Ok(())
}

fn derive_key(password: &str, salt: &[u8; SALT_LEN]) -> Result<[u8; 32], String> {
    let params =
        Params::new(ARGON_M_KIB, ARGON_T, ARGON_P, None).map_err(|e| format!("kdf params: {e}"))?;
    let a2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut key = [0u8; 32];
    a2.hash_password_into(password.as_bytes(), salt, &mut key)
        .map_err(|e| format!("kdf failed: {e}"))?;
    let out = key;
    wipe_bytes(&mut key); // the stack copy; `out` carries on
    Ok(out)
}

/// Seal a phrase under a fresh random salt+nonce.
pub fn encrypt_phrase(phrase: &str, password: &str) -> Result<Vec<u8>, String> {
    check_password(password)?;
    let mut salt = [0u8; SALT_LEN];
    let mut nonce = [0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut salt);
    OsRng.fill_bytes(&mut nonce);
    let key = derive_key(password, &salt)?;
    let cipher = ChaCha20Poly1305::new(Key::from_slice(&key));
    let mut ct = cipher
        .encrypt(Nonce::from_slice(&nonce), phrase.as_bytes())
        .map_err(|e| format!("seal failed: {e}"))?;
    let mut blob = Vec::with_capacity(4 + SALT_LEN + NONCE_LEN + ct.len());
    blob.extend_from_slice(ENC_MAGIC);
    blob.extend_from_slice(&salt);
    blob.extend_from_slice(&nonce);
    blob.append(&mut ct);
    Ok(blob)
}

/// Open a sealed blob. Wrong password and tampering both fail closed.
pub fn decrypt_phrase(blob: &[u8], password: &str) -> Result<String, String> {
    if blob.len() < 4 + SALT_LEN + NONCE_LEN + 16 || &blob[..4] != ENC_MAGIC {
        return Err("not a wallet vault (or corrupt)".to_string());
    }
    let mut salt = [0u8; SALT_LEN];
    let mut nonce = [0u8; NONCE_LEN];
    salt.copy_from_slice(&blob[4..4 + SALT_LEN]);
    nonce.copy_from_slice(&blob[4 + SALT_LEN..4 + SALT_LEN + NONCE_LEN]);
    let key = derive_key(password, &salt)?;
    let cipher = ChaCha20Poly1305::new(Key::from_slice(&key));
    let pt = cipher
        .decrypt(Nonce::from_slice(&nonce), &blob[4 + SALT_LEN + NONCE_LEN..])
        .map_err(|_| "wrong password or wallet file damaged".to_string())?;
    String::from_utf8(pt).map_err(|_| "wallet file damaged".to_string())
}

/// Legacy plaintext seed present without an encrypted vault.
pub fn is_legacy_plaintext(dir: &Path) -> bool {
    seed_path(dir).is_file() && !enc_path(dir).is_file()
}

/// Create a fresh 12-word wallet, sealed immediately. Refuses when any
/// wallet exists (restore replaces explicitly). Returns the phrase ONCE.
pub fn create_wallet_enc(dir: &Path, password: &str) -> Result<String, String> {
    if has_any_wallet(dir) {
        return Err("a wallet already exists here — restore to replace it".to_string());
    }
    let phrase = SeedPhrase::generate()?.phrase();
    write_0600(&enc_path(dir), &encrypt_phrase(&phrase, password)?)?;
    Ok(phrase)
}

/// Validate + install a backup phrase, sealed immediately.
pub fn restore_wallet_enc(dir: &Path, phrase: &str, password: &str) -> Result<(), String> {
    let phrase = phrase.trim().to_string();
    SeedPhrase::parse(&phrase)?; // checksum-validates
    write_0600(&enc_path(dir), &encrypt_phrase(&phrase, password)?)?;
    Ok(())
}

/// One-time upgrade: plaintext `wallet.seed` → sealed vault, then the
/// plaintext is wiped. Returns the phrase (caller unlocks the session).
pub fn migrate_plaintext(dir: &Path, password: &str) -> Result<String, String> {
    let phrase = reveal_phrase(dir)?;
    write_0600(&enc_path(dir), &encrypt_phrase(&phrase, password)?)?;
    // Best-effort shred of the plaintext (overwrite + remove).
    let p = seed_path(dir);
    if let Ok(mut f) = std::fs::OpenOptions::new().write(true).open(&p) {
        use std::io::Write;
        let len = std::fs::metadata(&p).map(|m| m.len() as usize).unwrap_or(0);
        let _ = f.write_all(&vec![0u8; len]);
        let _ = f.sync_all();
    }
    let _ = std::fs::remove_file(&p);
    Ok(phrase)
}

/// Change the vault password (needs the old one; re-seals, same seed).
pub fn change_password(dir: &Path, old: &str, new: &str) -> Result<(), String> {
    let phrase = decrypt_phrase(
        &std::fs::read(enc_path(dir)).map_err(|_| "no encrypted wallet".to_string())?,
        old,
    )?;
    write_0600(&enc_path(dir), &encrypt_phrase(&phrase, new)?)?;
    Ok(())
}

/// Unlock: decrypt the vault into a live session.
pub fn unlock_seed(dir: &Path, password: &str) -> Result<([u8; 64], String), String> {
    let blob = std::fs::read(enc_path(dir)).map_err(|_| "no encrypted wallet".to_string())?;
    let phrase = decrypt_phrase(&blob, password)?;
    Ok((SeedPhrase::parse(phrase.trim())?.seed(), phrase))
}

// ---- network -------------------------------------------------------------

/// The address family follows the CONNECTED node (testnet default).
pub fn network_of(rpc: &RpcClient) -> Result<Network, String> {
    let chain = rpc
        .call("getblockchaininfo", json!([]))?
        .get("chain")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    match chain.as_str() {
        // NOTE: `-regtest` boots mainnet-params in this fork (chain "main"),
        // and regtest shares mainnet's address prefixes — so "main" maps to
        // Mainnet everywhere (harmless: no mainnet chain exists yet, and the
        // prefixes coincide by consensus design).
        "test" => Ok(Network::Testnet),
        "main" => Ok(Network::Mainnet),
        "regtest" => Ok(Network::Regtest),
        other => Err(format!("unsupported chain '{other}'")),
    }
}

// ---- addresses -----------------------------------------------------------

pub struct DerivedAddr {
    pub change: u32,
    pub index: u32,
    pub address: String,
    pub script_hex: String,
}

/// Derive `count` addresses of one chain starting at `start`.
pub fn derive_range(
    seed: &[u8; 64],
    net: Network,
    change: u32,
    start: u32,
    count: u32,
) -> Result<Vec<DerivedAddr>, String> {
    let mut out = Vec::with_capacity(count as usize);
    for i in start..start.saturating_add(count) {
        let k = hd::external_key(seed, net, 0, change, i)?;
        let script = k.p2pkh_script(net);
        out.push(DerivedAddr {
            change,
            index: i,
            address: k.p2pkh_address(net),
            script_hex: hex_encode(&script),
        });
    }
    Ok(out)
}

fn hex_encode(b: &[u8]) -> String {
    const H: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(b.len() * 2);
    for &x in b {
        s.push(H[(x >> 4) as usize] as char);
        s.push(H[(x & 15) as usize] as char);
    }
    s
}

fn hex_decode(s: &str) -> Result<Vec<u8>, String> {
    if !s.len().is_multiple_of(2) {
        return Err("odd hex length".to_string());
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| "bad hex".to_string()))
        .collect()
}

/// Build a P2PKH/P2SH output script for a destination address on `net`.
pub fn dest_script(addr: &str, net: Network) -> Result<Vec<u8>, String> {
    let (ver, payload) = lm_keys::address::base58check_decode(addr)?;
    if payload.len() != 20 {
        return Err(format!("'{addr}' is not a P2PKH/P2SH address"));
    }
    if ver == net.p2pkh_version() {
        let mut s = vec![0x76, 0xa9, 0x14];
        s.extend_from_slice(&payload);
        s.extend_from_slice(&[0x88, 0xac]);
        Ok(s)
    } else if ver == net.p2sh_version() {
        Ok(lm_keys::address::p2sh_script(&payload))
    } else {
        Err(format!("'{addr}' is not a {} address", net_name(net)))
    }
}

fn net_name(net: Network) -> &'static str {
    match net {
        Network::Mainnet => "mainnet",
        Network::Testnet => "testnet",
        Network::Regtest => "regtest",
    }
}

/// "1.23456789" → satoshis. Rejects >8 decimals, zero, negatives, garbage.
pub fn parse_amount_lkyl(s: &str) -> Result<u64, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("amount is empty".to_string());
    }
    let (whole, frac) = match s.split_once('.') {
        Some((w, f)) => (w, f),
        None => (s, ""),
    };
    if whole.starts_with('-') || frac.starts_with('-') {
        return Err(format!("'{s}' is not a positive amount"));
    }
    if frac.len() > 8 {
        return Err("at most 8 decimals".to_string());
    }
    let w: u64 = if whole.is_empty() {
        0
    } else {
        whole
            .parse()
            .map_err(|_| format!("'{s}' is not a number"))?
    };
    let mut fpad = frac.to_string();
    while fpad.len() < 8 {
        fpad.push('0');
    }
    let f: u64 = if fpad.is_empty() {
        0
    } else {
        fpad.parse().map_err(|_| format!("'{s}' is not a number"))?
    };
    let sats = w
        .checked_mul(100_000_000)
        .and_then(|v| v.checked_add(f))
        .ok_or_else(|| "amount out of range".to_string())?;
    if sats == 0 {
        return Err("amount must be positive".to_string());
    }
    Ok(sats)
}

// ---- scan ----------------------------------------------------------------

/// One confirmed chain UTXO with everything needed to spend or display it.
#[derive(Clone)]
pub struct ScannedUtxo {
    pub txid_display: String,
    pub vout: u32,
    pub value: u64,
    pub script_hex: String,
    pub address: String,
    pub change: u32,
    pub index: u32,
    pub coinbase: bool,
    pub confirmations: u64,
}

pub struct ScanResult {
    /// Confirmed chain UTXOs (spendable set is derived by the caller).
    pub utxos: Vec<ScannedUtxo>,
    /// Signed net mempool effect in sats (outputs positive, spends negative).
    pub mempool_net: i64,
    /// All history txids (deduped, node order).
    pub txids: Vec<String>,
    /// Chain tip at scan time (confirmations recompute from this).
    pub tip: u64,
}

fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key)?.as_str().map(str::to_string)
}

fn u64_field(v: &Value, key: &str) -> Option<u64> {
    v.get(key)?.as_u64()
}

/// Persistent per-txid metadata (`<appdir>/txmeta.json`): the only things a
/// rescan would otherwise re-fetch one RPC at a time. `coinbase` never
/// changes; `height` lets confirmations recompute from the tip; `verbose`
/// caches the full decoded-once transaction for history. Reorg safety is
/// height-equality against the address index (a mismatch re-fetches); a
/// cache hit costs zero RPCs at any depth.
#[derive(Clone, Serialize, Deserialize, Default)]
struct TxMeta {
    #[serde(default)]
    coinbase: bool,
    #[serde(default)]
    height: u64,
    #[serde(default)]
    verbose: Option<Value>,
}

const TXMETA_FILE: &str = "txmeta.json";

fn load_txmeta(dir: &Path) -> HashMap<String, TxMeta> {
    std::fs::read_to_string(dir.join(TXMETA_FILE))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_txmeta(dir: &Path, map: &HashMap<String, TxMeta>) {
    if let Ok(s) = serde_json::to_string(map) {
        let _ = std::fs::write(dir.join(TXMETA_FILE), s);
    }
}

fn is_coinbase_tx(raw: &Value) -> bool {
    raw.get("vin")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .map(|v| v.get("coinbase").is_some())
        .unwrap_or(false)
}

/// Scan the wallet's addresses: UTXO set + mempool + history + usage.
/// `recv_next`/`change_next` are the persisted cursors; the scan always
/// covers `0..cursor+GAP` so nothing already shown is ever missed.
/// Per-txid metadata (coinbase flag, verbose tx) comes from the persistent
/// cache — only unknown or very recent txids cost an RPC.
pub fn scan(
    rpc: &RpcClient,
    seed: &[u8; 64],
    net: Network,
    recv_next: u32,
    change_next: u32,
    dir: &Path,
) -> Result<ScanResult, String> {
    let tip = rpc
        .call("getblockcount", json!([]))?
        .as_u64()
        .ok_or("getblockcount: unexpected")?;
    let mut derived = derive_range(seed, net, 0, 0, recv_next.saturating_add(GAP))?;
    derived.extend(derive_range(
        seed,
        net,
        1,
        0,
        change_next.saturating_add(GAP),
    )?);
    let addrs: Vec<String> = derived.iter().map(|d| d.address.clone()).collect();
    let by_addr: HashMap<&str, &DerivedAddr> =
        derived.iter().map(|d| (d.address.as_str(), d)).collect();

    // Confirmed UTXOs (one batched call).
    let raw_utxos = rpc.call("getaddressutxos", json!([{"addresses": addrs}]))?;
    let mut meta = load_txmeta(dir);
    let mut dirty = false;
    let mut utxos = Vec::new();
    if let Some(arr) = raw_utxos.as_array() {
        for u in arr {
            let addr = match str_field(u, "address") {
                Some(a) => a,
                None => continue,
            };
            let d = match by_addr.get(addr.as_str()) {
                Some(d) => d,
                None => continue, // not ours (shouldn't happen)
            };
            let txid = match str_field(u, "txid") {
                Some(t) => t,
                None => continue,
            };
            // Confirmations from the index height (no RPC); the coinbase
            // flag from the persistent cache (one fetch per txid, ever —
            // a tx's coinbase-ness never changes). Young/reorg-shifted
            // entries are re-fetched for safety.
            let idx_height = u64_field(u, "height").unwrap_or(0);
            let (coinbase, confirmations) = match meta.get(&txid) {
                // Height equality IS the reorg check: getaddressutxos
                // returns the CURRENT index height, so a match means the
                // entry is unchanged (coinbase-ness never changes, and
                // confirmations recompute from the tip). A reorg shows up
                // as a mismatch and re-fetches; a mempool→confirm
                // transition is a 0→height mismatch too. The old extra
                // depth clause re-fetched every UTXO younger than 12
                // confirmations on EVERY refresh — with per-minute pool
                // payouts that was ~a dozen RPCs per refresh (the
                // "balances take a while" report).
                Some(m) if m.height == idx_height && idx_height > 0 => {
                    (m.coinbase, tip.saturating_sub(idx_height).saturating_add(1))
                }
                _ => {
                    let raw = rpc.call("getrawtransaction", json!([txid, 1]))?;
                    let cb = is_coinbase_tx(&raw);
                    let h = raw
                        .get("height")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(idx_height);
                    let confs = raw
                        .get("confirmations")
                        .and_then(|v| v.as_u64())
                        .unwrap_or_else(|| {
                            if h > 0 {
                                tip.saturating_sub(h).saturating_add(1)
                            } else {
                                0
                            }
                        });
                    meta.insert(
                        txid.clone(),
                        TxMeta {
                            coinbase: cb,
                            height: h,
                            verbose: Some(raw),
                        },
                    );
                    dirty = true;
                    (cb, confs)
                }
            };
            utxos.push(ScannedUtxo {
                txid_display: txid,
                vout: u64_field(u, "outputIndex").unwrap_or(0) as u32,
                value: u64_field(u, "satoshis").unwrap_or(0),
                // Chain truth, not the derived template: guarded (StakeGuard
                // CC) outputs decode to our address in the index but carry a
                // nonstandard script — signing P2PKH-style for them builds a
                // tx the mempool rejects ("nonstandard transaction input").
                script_hex: str_field(u, "script").unwrap_or_else(|| d.script_hex.clone()),
                address: addr,
                change: d.change,
                index: d.index,
                coinbase,
                confirmations,
            });
        }
    }
    if dirty {
        save_txmeta(dir, &meta);
    }

    // Mempool effect (best-effort; absent on old nodes).
    let mempool_net = rpc
        .call("getaddressmempool", json!([{"addresses": addrs}]))
        .ok()
        .and_then(|v| v.as_array().cloned())
        .map(|arr| {
            arr.iter()
                .map(|e| e.get("satoshis").and_then(|v| v.as_i64()).unwrap_or(0))
                .sum()
        })
        .unwrap_or(0);

    // History (one batched call, deduped).
    let mut txids = Vec::new();
    let mut seen = HashSet::new();
    if let Some(arr) = rpc
        .call("getaddresstxids", json!([{"addresses": addrs}]))
        .ok()
        .and_then(|v| v.as_array().cloned())
    {
        for t in arr {
            if let Some(id) = t.as_str() {
                if seen.insert(id.to_string()) {
                    txids.push(id.to_string());
                }
            }
        }
    }

    // Usage = any own script appearing in history (filled by activity()).
    Ok(ScanResult {
        utxos,
        mempool_net,
        txids,
        tip,
    })
}

/// Full restore scan: windowed history search (BIP44 discovery) over both
/// chains. Returns fresh cursors (highest used index + 1, else 0).
pub fn full_scan_used(
    rpc: &RpcClient,
    seed: &[u8; 64],
    net: Network,
) -> Result<(u32, u32, usize), String> {
    let mut found = 0usize;
    let mut cursors = (0u32, 0u32);
    for (chain, slot) in [(0u32, 0usize), (1u32, 1usize)] {
        let mut start = 0u32;
        let mut max_used: Option<u32> = None;
        for _ in 0..100 {
            let addrs = derive_range(seed, net, chain, start, GAP)?;
            let names: Vec<String> = addrs.iter().map(|d| d.address.clone()).collect();
            // Window-level check first (one call); drill per-address only
            // when the window has any history.
            let any: Vec<String> = rpc
                .call("getaddresstxids", json!([{"addresses": names}]))
                .ok()
                .and_then(|v| {
                    v.as_array().map(|a| {
                        a.iter()
                            .filter_map(|t| t.as_str().map(str::to_string))
                            .collect()
                    })
                })
                .unwrap_or_default();
            if any.is_empty() {
                break;
            }
            for d in &addrs {
                let mine: Vec<String> = rpc
                    .call("getaddresstxids", json!([{"addresses": [d.address]}]))
                    .ok()
                    .and_then(|v| {
                        v.as_array().map(|a| {
                            a.iter()
                                .filter_map(|t| t.as_str().map(str::to_string))
                                .collect()
                        })
                    })
                    .unwrap_or_default();
                if !mine.is_empty() {
                    max_used = Some(d.index);
                    found += 1;
                }
            }
            start += GAP;
        }
        let next = max_used.map(|m| m + 1).unwrap_or(0);
        if slot == 0 {
            cursors.0 = next;
        } else {
            cursors.1 = next;
        }
    }
    Ok((cursors.0, cursors.1, found))
}

// ---- balances + activity --------------------------------------------------

pub struct Balances {
    pub available: u64,
    pub unconfirmed: i64,
    pub immature: u64,
    pub txcount: usize,
    /// Mature but relay-unspendable (StakeGuard-guarded PoS rewards):
    /// consensus-valid, stakeable, but no wallet path can send them —
    /// shown separately so "balance" never promises them for sends.
    pub guarded: u64,
}

/// Relay policy (`AreInputsStandard`) only spends P2PKH/P2PK prevouts.
/// Anything else (notably StakeGuard CC reward outputs, which decode to
/// our address in the index) must be excluded from normal sends or the
/// node rejects the whole tx with "nonstandard transaction input".
pub fn is_relay_spendable(script_hex: &str) -> bool {
    // ASCII first: the byte slices below are only safe on hex text.
    if !script_hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return false;
    }
    let s = script_hex;
    let is_p2pkh = s.len() == 50 && s.starts_with("76a914") && s.ends_with("88ac");
    let is_p2pk = (s.len() == 70 && s.starts_with("21") && s.ends_with("ac"))
        || (s.len() == 134 && s.starts_with("41") && s.ends_with("ac"));
    is_p2pkh || is_p2pk
}

/// Split the scan into the Overview cards. `tip` = current chain height.
pub fn balances_of(scan: &ScanResult, tip: u64) -> Balances {
    let _ = tip;
    let mut available = 0u64;
    let mut immature = 0u64;
    let mut guarded = 0u64;
    for u in &scan.utxos {
        if u.coinbase && u.confirmations < COINBASE_MATURITY {
            immature += u.value;
        } else if !is_relay_spendable(&u.script_hex) {
            guarded += u.value;
        } else {
            available += u.value;
        }
    }
    Balances {
        available,
        unconfirmed: scan.mempool_net,
        immature,
        txcount: scan.txids.len(),
        guarded,
    }
}

pub struct ActivityRow {
    pub txid: String,
    pub category: String, // "receive" | "send" | "self"
    pub amount: f64,      // signed coins (sends negative, like the node)
    pub address: String,  // counterparty (first external) or first own
    pub time: u64,
    pub confirmations: u64,
}

#[derive(Clone)]
pub(crate) struct DecodedTx {
    vin: Vec<(Option<String>, Option<u32>)>, // (prev txid, vout)
    vouts: Vec<(u64, String, Option<String>)>, // (valueSat, script_hex, address?)
    time: u64,
    confirmations: u64,
    height: Option<u64>,
}

fn decode_tx(raw: &Value) -> DecodedTx {
    let mut vin = Vec::new();
    if let Some(arr) = raw.get("vin").and_then(|v| v.as_array()) {
        for i in arr {
            if i.get("coinbase").is_some() {
                vin.push((None, None));
            } else {
                vin.push((
                    i.get("txid").and_then(|v| v.as_str()).map(str::to_string),
                    i.get("vout").and_then(|v| v.as_u64()).map(|v| v as u32),
                ));
            }
        }
    }
    let mut vouts = Vec::new();
    if let Some(arr) = raw.get("vout").and_then(|v| v.as_array()) {
        for o in arr {
            let value = o
                .get("valueSat")
                .and_then(|v| v.as_u64())
                .unwrap_or_else(|| {
                    (o.get("value").and_then(|v| v.as_f64()).unwrap_or(0.0) * 100_000_000.0).round()
                        as u64
                });
            let sp = o.get("scriptPubKey");
            let script_hex = sp
                .and_then(|s| s.get("hex"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let addr = sp
                .and_then(|s| s.get("addresses"))
                .and_then(|v| v.as_array())
                .and_then(|a| a.first())
                .and_then(|v| v.as_str())
                .map(str::to_string);
            vouts.push((value, script_hex, addr));
        }
    }
    DecodedTx {
        vin,
        vouts,
        time: raw
            .get("blocktime")
            .or_else(|| raw.get("time"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        confirmations: raw
            .get("confirmations")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        height: raw.get("height").and_then(|v| v.as_u64()),
    }
}

/// Pure row builder (unit-tested): direction/amount from a decoded tx.
/// `prev` resolves (prev_txid, vout) → (value_sats, script_hex) or None.
/// Appends every touched own (change, index) to `used`.
pub fn row_for_tx(
    txid: &str,
    tx: &DecodedTx,
    prev: PrevLookup<'_>,
    own_scripts: &HashMap<String, (u32, u32)>,
    used: &mut Vec<(u32, u32)>,
) -> ActivityRow {
    let mut touch = |change: u32, index: u32| {
        if !used.contains(&(change, index)) {
            used.push((change, index));
        }
    };
    let mut in_own = 0u64;
    let mut out_own = 0u64;
    let mut out_ext = 0u64;
    let mut first_own: Option<String> = None;
    let mut first_ext: Option<String> = None;
    for (ptxid, pvout) in &tx.vin {
        let (Some(pt), Some(pv)) = (ptxid, pvout) else {
            continue;
        };
        if let Some((value, script)) = prev(pt, *pv) {
            if let Some((ch, ix)) = own_scripts.get(script.as_str()) {
                in_own += value;
                touch(*ch, *ix);
            }
        }
    }
    for (value, script, addr) in &tx.vouts {
        match own_scripts.get(script.as_str()) {
            Some((ch, ix)) => {
                out_own += value;
                touch(*ch, *ix);
                if first_own.is_none() {
                    first_own = addr.clone().or_else(|| Some(format!("script:{script}")));
                }
            }
            None => {
                out_ext += value;
                if first_ext.is_none() {
                    first_ext = addr.clone().or_else(|| Some(format!("script:{script}")));
                }
            }
        }
    }
    let (category, amount_sats, address) = if in_own > 0 && out_ext > 0 {
        ("send", -(out_ext as i64), first_ext.unwrap_or_default())
    } else if in_own > 0 {
        ("self", 0, first_own.clone().unwrap_or_default())
    } else {
        (
            "receive",
            out_own as i64,
            first_own.clone().unwrap_or_default(),
        )
    };
    ActivityRow {
        txid: txid.to_string(),
        category: category.to_string(),
        amount: amount_sats as f64 / 100_000_000.0,
        address,
        time: tx.time,
        confirmations: tx.confirmations,
    }
}

/// Net fee in sats when every input resolves, else None.
pub fn fee_of(tx: &DecodedTx, prev: PrevLookup<'_>) -> Option<u64> {
    let mut ins = 0u64;
    for (ptxid, pvout) in &tx.vin {
        let (Some(pt), Some(pv)) = (ptxid, pvout) else {
            return None; // coinbase: no fee
        };
        ins += prev(pt, *pv)?.0;
    }
    let outs: u64 = tx.vouts.iter().map(|(v, _, _)| v).sum();
    ins.checked_sub(outs)
}

/// Resolve one previous output (value + script) for direction and fee
/// math. None when the prev tx is unavailable (pruned/unconfirmed edge).
/// Previous-output lookup over a primed cache (free function so the
/// cache stays mutable for priming while lookups borrow it briefly).
type PrevLookup<'a> = &'a dyn Fn(&str, u32) -> Option<(u64, String)>;

fn prev_lookup(
    cache: &HashMap<(String, u32), Option<(u64, String)>>,
    pt: &str,
    pv: u32,
) -> Option<(u64, String)> {
    cache.get(&(pt.to_string(), pv)).cloned().unwrap_or(None)
}

/// One page of wallet history plus cursor-advance hints.
pub struct ActivityPage {
    pub rows: Vec<ActivityRow>,
    pub used_recv_max: Option<u32>,
    pub used_change_max: Option<u32>,
}

/// Build activity rows for the wallet's history, tracking used indices for
/// cursor advance.
/// Fetch one verbose tx, serving from the persistent cache first. Only
/// unknown txids cost an RPC — history converges to zero calls.
fn cached_verbose(
    rpc: &RpcClient,
    meta: &mut HashMap<String, TxMeta>,
    dirty: &mut bool,
    txid: &str,
) -> Result<Value, String> {
    if let Some(m) = meta.get(txid) {
        if let Some(v) = &m.verbose {
            return Ok(v.clone());
        }
    }
    let v = rpc.call("getrawtransaction", json!([txid, 1]))?;
    let h = v.get("height").and_then(|x| x.as_u64()).unwrap_or(0);
    meta.insert(
        txid.to_string(),
        TxMeta {
            coinbase: is_coinbase_tx(&v),
            height: h,
            verbose: Some(v.clone()),
        },
    );
    *dirty = true;
    Ok(v)
}

/// Resolve one prevout through the shared verbose cache.
fn cached_prev(
    rpc: &RpcClient,
    meta: &mut HashMap<String, TxMeta>,
    dirty: &mut bool,
    pt: &str,
    pv: u32,
) -> Option<(u64, String)> {
    cached_verbose(rpc, meta, dirty, pt).ok().and_then(|p| {
        decode_tx(&p)
            .vouts
            .iter()
            .enumerate()
            .find(|(i, _)| *i as u32 == pv)
            .map(|(_, (v, s, _))| (*v, s.clone()))
    })
}

pub fn activity(
    rpc: &RpcClient,
    txids: &[String],
    own_scripts: &HashMap<String, (u32, u32)>,
    limit: usize,
    tip: u64,
    dir: &Path,
) -> Result<ActivityPage, String> {
    let mut meta = load_txmeta(dir);
    let mut dirty = false;
    let mut prev_cache: HashMap<(String, u32), Option<(u64, String)>> = HashMap::new();
    let mut rows = Vec::new();
    let mut used: Vec<(u32, u32)> = Vec::new();
    for txid in txids.iter().take(limit) {
        let raw = cached_verbose(rpc, &mut meta, &mut dirty, txid)?;
        let mut tx = decode_tx(&raw);
        // A cached entry without a height was captured while unconfirmed
        // (txmeta serves the verbose forever): re-fetch it once so it can
        // land; otherwise it would freeze at 0 confirmations.
        if tx.height.unwrap_or(0) == 0 {
            let fresh = rpc.call("getrawtransaction", json!([txid, 1]))?;
            meta.insert(
                txid.clone(),
                TxMeta {
                    coinbase: is_coinbase_tx(&fresh),
                    height: fresh.get("height").and_then(|v| v.as_u64()).unwrap_or(0),
                    verbose: Some(fresh.clone()),
                },
            );
            dirty = true;
            tx = decode_tx(&fresh);
        }
        // Confirmations must track the CURRENT tip: the cached verbose's
        // own `confirmations` value is frozen at first-fetch depth (a
        // payout cached at 1 conf read "1" forever — the "confirmations
        // look off" report). Recompute from the cached height + live tip.
        // (In-chain reorgs could leave the cached height stale until the
        // entry is re-fetched; resets drop txids from the index entirely.)
        if let Some(h) = tx.height {
            if h > 0 {
                tx.confirmations = tip.saturating_sub(h).saturating_add(1);
            }
        }
        // Prime the prev cache through the shared verbose cache.
        for (ptxid, pvout) in &tx.vin {
            if let (Some(pt), Some(pv)) = (ptxid, pvout) {
                prev_cache
                    .entry((pt.clone(), *pv))
                    .or_insert_with(|| cached_prev(rpc, &mut meta, &mut dirty, pt, *pv));
            }
        }
        let lookup = |pt: &str, pv: u32| prev_lookup(&prev_cache, pt, pv);
        rows.push(row_for_tx(txid, &tx, &lookup, own_scripts, &mut used));
    }
    // Newest first (node order is oldest-first; UI slices the front).
    rows.sort_by(|a, b| b.time.cmp(&a.time).then(b.txid.cmp(&a.txid)));
    let max_for = |ch: u32| used.iter().filter(|(c, _)| *c == ch).map(|(_, i)| *i).max();
    if dirty {
        save_txmeta(dir, &meta);
    }
    Ok(ActivityPage {
        rows,
        used_recv_max: max_for(0),
        used_change_max: max_for(1),
    })
}

/// Fee (sats/KB) for wallet sends: the network floor with headroom.
pub use lm_keys::sign::DEFAULT_FEE_RATE_SAT_PER_KB;

/// Fee-rate bounds for the Send chooser (sat/kB). Floor = the node's
/// relay minimum (below it the mempool refuses the tx); cap = fat-finger
/// guard (100x the default is urgency enough on any chain state).
/// RBF is deliberately NOT offered: the node disables replacement
/// (`main.cpp` "Disable replacement feature for now"), so a bump would
/// just double-spend into a second mempool entry — the chooser sets the
/// rate up front instead.
pub const MIN_FEE_RATE_SAT_PER_KB: u64 = 100;
pub const MAX_FEE_RATE_SAT_PER_KB: u64 = 100_000;

/// Validate an explicit fee rate (the chooser + custom field).
pub fn check_fee_rate(rate: u64) -> Result<u64, String> {
    if rate < MIN_FEE_RATE_SAT_PER_KB {
        return Err(format!(
            "fee rate {rate} sat/kB is below the network relay minimum ({MIN_FEE_RATE_SAT_PER_KB} sat/kB)"
        ));
    }
    if rate > MAX_FEE_RATE_SAT_PER_KB {
        return Err(format!(
            "fee rate {rate} sat/kB exceeds the {MAX_FEE_RATE_SAT_PER_KB} sat/kB sanity cap"
        ));
    }
    Ok(rate)
}

pub struct SendResult {
    pub txid: String,
    pub fee: u64,
    pub change: u64,
    /// Change index consumed, if a change output was created.
    pub change_used: Option<u32>,
}

/// A locally built + signed payment that has NOT been broadcast (review
/// flow: preview first, confirm second). The txid is a commitment to the
/// exact bytes — confirm cannot broadcast anything else.
pub struct SendDraft {
    pub hex: String,
    pub txid: String,
    pub fee: u64,
    pub change: u64,
    /// Change index consumed, if a change output was created.
    pub change_used: Option<u32>,
}

/// The signature-hash epoch the connected chain is in (drives the tx
/// format + sighash of locally built spends). Read from the node's
/// upgrade status (`getblockchaininfo.upgrades` keys ARE the consensus
/// branch ids). Falls back to `Sprout` when the node can't answer: the
/// legacy format is correct pre-activation, and post-activation a
/// missed query can only produce a draft the node refuses to broadcast
/// (no fund risk). ZIP-143/243 themselves are vector-pinned in lm-keys
/// (tests/zip143.rs + zip243.rs).
pub fn spend_epoch(rpc: &RpcClient) -> lm_keys::sign::Epoch {
    let info = rpc.call("getblockchaininfo", serde_json::json!([]));
    let active = |branch: &str| -> bool {
        info.as_ref()
            .ok()
            .and_then(|v| v.get("upgrades"))
            .and_then(|u| u.get(branch))
            .and_then(|b| b.get("status"))
            .and_then(|s| s.as_str())
            == Some("active")
    };
    if active("76b809bb") {
        lm_keys::sign::Epoch::Sapling(lm_keys::zip243::SAPLING_BRANCH_ID)
    } else if active("5ba81b19") {
        lm_keys::sign::Epoch::Overwinter(lm_keys::zip243::OVERWINTER_BRANCH_ID)
    } else {
        lm_keys::sign::Epoch::Sprout
    }
}

/// Build (local keys) and sign a payment — no network touch.
/// Only mature confirmed relay-spendable UTXOs fund it (coinbases need
/// 100 conf; StakeGuard-guarded rewards are excluded — the mempool
/// rejects any tx spending them); unconfirmed change is never chained
/// in v1.
#[allow(clippy::too_many_arguments)]
pub fn prepare_payment(
    seed: &[u8; 64],
    net: Network,
    utxos: &[ScannedUtxo],
    dest_addr: &str,
    amount_sats: u64,
    change_next: u32,
    epoch: lm_keys::sign::Epoch,
    fee_rate_sat_per_kb: u64,
) -> Result<SendDraft, String> {
    let dest_script = dest_script(dest_addr, net)?;
    let mut spend = Vec::new();
    let mut skipped_guarded = 0u64;
    for u in utxos {
        let mature = (!u.coinbase && u.confirmations >= 1)
            || (u.coinbase && u.confirmations >= COINBASE_MATURITY);
        if !mature {
            continue;
        }
        if !is_relay_spendable(&u.script_hex) {
            skipped_guarded += u.value;
            continue;
        }
        let secret = *hd::external_key(seed, net, 0, u.change, u.index)?.secret();
        spend.push(sign::SpendUtxo {
            txid: tx::parse_txid_display(&u.txid_display)?,
            vout: u.vout,
            value: u.value,
            script: hex_decode(&u.script_hex)?,
            secret,
        });
    }
    if spend.is_empty() {
        if skipped_guarded > 0 {
            return Err(format!(
                "no relay-spendable confirmed funds ({:.8} locked in stake-guarded rewards; coinbases mature after 100 blocks)",
                skipped_guarded as f64 / 100_000_000.0
            ));
        }
        return Err("no spendable confirmed funds (coinbases mature after 100 blocks)".to_string());
    }
    let change_key = hd::external_key(seed, net, 0, 1, change_next)?;
    let change_script = change_key.p2pkh_script(net);
    let built = sign::build_spend(
        &spend,
        &dest_script,
        amount_sats,
        &change_script,
        check_fee_rate(fee_rate_sat_per_kb)?,
        epoch,
    )?;
    Ok(SendDraft {
        hex: built.hex,
        txid: built.txid,
        fee: built.fee,
        change: built.change,
        change_used: if built.change > 0 {
            Some(change_next)
        } else {
            None
        },
    })
}

/// Build (local keys), sign (local keys) and broadcast a payment.
#[allow(clippy::too_many_arguments)]
pub fn send_payment(
    rpc: &RpcClient,
    seed: &[u8; 64],
    net: Network,
    utxos: &[ScannedUtxo],
    dest_addr: &str,
    amount_sats: u64,
    change_next: u32,
    fee_rate_sat_per_kb: u64,
) -> Result<SendResult, String> {
    let epoch = spend_epoch(rpc);
    let draft = prepare_payment(seed, net, utxos, dest_addr, amount_sats, change_next, epoch, fee_rate_sat_per_kb)?;
    let txid: String = broadcast_payment(rpc, &draft.hex, &draft.txid)?;
    Ok(SendResult {
        txid,
        fee: draft.fee,
        change: draft.change,
        change_used: draft.change_used,
    })
}

/// Broadcast a previously built + signed payment. The node must return
/// the committed txid — anything else is a hard failure (no silent
/// substitution).
pub fn broadcast_payment(rpc: &RpcClient, hex: &str, expect_txid: &str) -> Result<String, String> {
    let txid: String = rpc
        .call("sendrawtransaction", json!([hex]))
        .map_err(|e| format!("broadcast rejected: {e}"))?
        .as_str()
        .ok_or_else(|| "sendrawtransaction: unexpected result".to_string())?
        .to_string();
    if txid != expect_txid {
        return Err("node returned a different txid than built".to_string());
    }
    Ok(txid)
}

/// The address currently shown for receiving (cursor, not yet advanced).
pub fn current_receive(seed: &[u8; 64], net: Network, recv_next: u32) -> Result<String, String> {
    receive_address(seed, net, recv_next)
}

/// Receive address at an explicit index.
pub fn receive_address(seed: &[u8; 64], net: Network, index: u32) -> Result<String, String> {
    Ok(hd::external_key(seed, net, 0, 0, index)?.p2pkh_address(net))
}

/// Address → (change, index) map over the scanned window (for matching).
pub fn own_script_map(
    seed: &[u8; 64],
    net: Network,
    recv_next: u32,
    change_next: u32,
) -> Result<HashMap<String, (u32, u32)>, String> {
    let mut map = HashMap::new();
    for d in derive_range(seed, net, 0, 0, recv_next.saturating_add(GAP))? {
        map.insert(d.script_hex, (0, d.index));
    }
    for d in derive_range(seed, net, 1, 0, change_next.saturating_add(GAP))? {
        map.insert(d.script_hex, (1, d.index));
    }
    Ok(map)
}

/// Full detail for one transaction (same shape the UI already renders).
pub fn tx_detail(
    rpc: &RpcClient,
    txid: &str,
    own_scripts: &HashMap<String, (u32, u32)>,
    dir: &Path,
) -> Result<Value, String> {
    let mut meta = load_txmeta(dir);
    let mut dirty = false;
    let raw = cached_verbose(rpc, &mut meta, &mut dirty, txid)?;
    let mut tx = decode_tx(&raw);
    // Confirmations from the current tip, not the frozen first-fetch value
    // (best-effort: keep the cached value if the tip call fails).
    if let Some(h) = tx.height {
        if h > 0 {
            if let Ok(v) = rpc.call("getblockcount", json!([])) {
                if let Some(tip) = v.as_u64() {
                    tx.confirmations = tip.saturating_sub(h).saturating_add(1);
                }
            }
        }
    }
    // Resolve previous outputs for direction + fee.
    let mut prevs: HashMap<(String, u32), Option<(u64, String)>> = HashMap::new();
    for (ptxid, pvout) in &tx.vin {
        if let (Some(pt), Some(pv)) = (ptxid, pvout) {
            prevs
                .entry((pt.clone(), *pv))
                .or_insert_with(|| cached_prev(rpc, &mut meta, &mut dirty, pt, *pv));
        }
    }
    if dirty {
        save_txmeta(dir, &meta);
    }
    let lookup = |pt: &str, pv: u32| prev_lookup(&prevs, pt, pv);
    let mut used = Vec::new();
    let row = row_for_tx(txid, &tx, &lookup, own_scripts, &mut used);
    let fee = fee_of(&tx, &lookup);
    // Per-counterparty entries (node gettransaction style).
    let mut details = Vec::new();
    // Re-walk for entries (row_for_tx collapses to one row; detail expands).
    let mut in_own_any = false;
    for (ptxid, pvout) in &tx.vin {
        if let (Some(pt), Some(pv)) = (ptxid, pvout) {
            if let Some((_, script)) = lookup(pt, *pv) {
                if own_scripts.contains_key(script.as_str()) {
                    in_own_any = true;
                }
            }
        }
    }
    for (value, script, addr) in &tx.vouts {
        let entry_addr = addr.clone().unwrap_or_else(|| format!("script:{script}"));
        if own_scripts.contains_key(script.as_str()) {
            details.push(json!({
                "category": "receive",
                "address": entry_addr,
                "amount": *value as f64 / 100_000_000.0,
            }));
        } else if in_own_any {
            details.push(json!({
                "category": "send",
                "address": entry_addr,
                "amount": -(*value as f64) / 100_000_000.0,
            }));
        }
    }
    Ok(json!({
        "txid": txid,
        "amount": row.amount,
        "fee": fee.map(|f| f as f64 / 100_000_000.0),
        "confirmations": tx.confirmations,
        "time": tx.time,
        "blockheight": tx.height,
        "details": details,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn amount_parsing() {
        assert_eq!(parse_amount_lkyl("1").unwrap(), 100_000_000);
        assert_eq!(parse_amount_lkyl("0.00000001").unwrap(), 1);
        assert_eq!(parse_amount_lkyl(" 2.5 ").unwrap(), 250_000_000);
        assert_eq!(parse_amount_lkyl("0.1").unwrap(), 10_000_000);
        assert!(parse_amount_lkyl("0").is_err());
        assert!(parse_amount_lkyl("-1").is_err());
        assert!(parse_amount_lkyl("abc").is_err());
        assert!(parse_amount_lkyl("1.123456789").is_err());
        assert!(parse_amount_lkyl("").is_err());
    }

    #[test]
    fn txmeta_cache_roundtrips() {
        let dir = std::env::temp_dir().join(format!("lk-wmeta-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("tmpdir");
        assert!(load_txmeta(&dir).is_empty());
        let mut m = HashMap::new();
        m.insert(
            "tx1".to_string(),
            TxMeta {
                coinbase: true,
                height: 10,
                verbose: Some(json!({"txid": "tx1"})),
            },
        );
        save_txmeta(&dir, &m);
        let back = load_txmeta(&dir);
        assert_eq!(back.len(), 1);
        assert!(back["tx1"].coinbase);
        assert_eq!(back["tx1"].height, 10);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn activity_serves_history_from_cache_without_rpc() {
        let dir = std::env::temp_dir().join(format!("lk-wcache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("tmpdir");
        // One coinbase payout to our address, already cached exactly as a
        // prior scan would have left it.
        let txid = "a".repeat(64);
        let addr = "mTestAddr1111111111111111111111111";
        let script = "76a914001122334455667788990011223344556677889988ac";
        let verbose = json!({
            "txid": txid,
            "vin": [{"coinbase": "04ffff001d0102", "sequence": 4294967295u64}],
            "vout": [{"value": 0.5, "valueSat": 50_000_000u64,
                      "scriptPubKey": {"hex": script, "addresses": [addr]}}],
            "blocktime": 1792000000u64,
            "confirmations": 150u64,
            "height": 10u64,
        });
        let mut meta = HashMap::new();
        meta.insert(
            txid.clone(),
            TxMeta {
                coinbase: true,
                height: 10,
                verbose: Some(verbose),
            },
        );
        save_txmeta(&dir, &meta);
        let mut map = HashMap::new();
        map.insert(script.to_string(), (0u32, 0u32));
        // Dead endpoint: any RPC attempt fails, so this only passes on a
        // pure cache hit (the whole point of the fix).
        let dead = RpcClient::new("http://127.0.0.1:9", "", "");
        let page =
            activity(&dead, std::slice::from_ref(&txid), &map, 25, 20, &dir).expect("cache hit");
        assert_eq!(page.rows.len(), 1);
        assert_eq!(page.rows[0].txid, txid);
        assert_eq!(page.rows[0].category, "receive");
        // Confirmations recompute from the CURRENT tip (20), not the
        // frozen first-fetch value in the cache (150): 20 - height 10 + 1.
        assert_eq!(page.rows[0].confirmations, 11);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dest_script_builds_p2pkh_and_p2sh() {
        // Testnet m/n = P2PKH (111), 2 = P2SH (196).
        let p2pkh =
            dest_script("mvSvTtvD9H9fkgi8MGDyLALgRaR2LhnWFM", Network::Testnet).expect("p2pkh");
        assert_eq!(p2pkh.len(), 25);
        assert_eq!(
            (p2pkh[0], p2pkh[1], p2pkh[2], p2pkh[23], p2pkh[24]),
            (0x76, 0xa9, 0x14, 0x88, 0xac)
        );
        // Wrong network rejected.
        assert!(dest_script("mvSvTtvD9H9fkgi8MGDyLALgRaR2LhnWFM", Network::Mainnet).is_err());
        // Garbage rejected.
        assert!(dest_script("not-an-address", Network::Testnet).is_err());
    }

    #[test]
    fn balances_split_maturity() {
        // Real P2PKH template; the guarded entry mirrors a StakeGuard CC
        // reward output (decodes to our address, relay-unspendable).
        const P2PKH: &str = "76a914abababababababababababababababababababab88ac";
        const GUARDED: &str = "2ea22c80207a4029b3230f7943957f81f24a9f438e20a0ede2779bcb12be228be5ab07debd81031210008203000401cc4c90";
        assert!(is_relay_spendable(P2PKH));
        assert!(!is_relay_spendable(GUARDED));
        assert!(!is_relay_spendable("s"));
        assert!(!is_relay_spendable(""));
        let scan = ScanResult {
            utxos: vec![
                ScannedUtxo {
                    txid_display: "a".into(),
                    vout: 0,
                    value: 1000,
                    script_hex: P2PKH.into(),
                    address: "a".into(),
                    change: 0,
                    index: 0,
                    coinbase: false,
                    confirmations: 5,
                },
                ScannedUtxo {
                    txid_display: "b".into(),
                    vout: 0,
                    value: 2000,
                    script_hex: P2PKH.into(),
                    address: "b".into(),
                    change: 0,
                    index: 1,
                    coinbase: true,
                    confirmations: 50,
                },
                ScannedUtxo {
                    txid_display: "c".into(),
                    vout: 0,
                    value: 4000,
                    script_hex: P2PKH.into(),
                    address: "c".into(),
                    change: 0,
                    index: 2,
                    coinbase: true,
                    confirmations: 100,
                },
                ScannedUtxo {
                    txid_display: "g".into(),
                    vout: 0,
                    value: 7000,
                    script_hex: GUARDED.into(),
                    address: "g".into(),
                    change: 0,
                    index: 3,
                    coinbase: false,
                    confirmations: 200,
                },
            ],
            mempool_net: -500,
            txids: vec!["a".into(), "b".into(), "c".into(), "g".into()],
            tip: 200,
        };
        let b = balances_of(&scan, 1000);
        assert_eq!(
            (b.available, b.immature, b.guarded, b.unconfirmed, b.txcount),
            (5000, 2000, 7000, -500, 4)
        );
    }

    /// Stake-guarded (nonstandard-script) UTXOs never fund a normal send:
    /// the mempool rejects the whole tx otherwise ("nonstandard
    /// transaction input"). Mixed sets fund from P2PKH only; guarded-only
    /// sets fail with the guarded message, not a bare maturity error.
    /// The fee chooser scales the fee linearly with the rate and the
    /// bounds reject relay-dust and fat-finger burns. RBF is not offered
    /// (the node disables replacement), so the rate is set up front.
    #[test]
    fn prepare_payment_honors_explicit_fee_rate() {
        const P2PKH: &str = "76a914abababababababababababababababababababab88ac";
        let seed = [9u8; 64];
        let utxo = ScannedUtxo {
            txid_display: "cc".repeat(32),
            vout: 0,
            value: 1_000_000,
            script_hex: P2PKH.into(),
            address: "mX".into(),
            change: 0,
            index: 0,
            coinbase: false,
            confirmations: 50,
        };
        let dest = "mvSvTtvD9H9fkgi8MGDyLALgRaR2LhnWFM";
        let epoch = lm_keys::sign::Epoch::Sprout;
        let base = prepare_payment(&seed, Network::Testnet, &[utxo.clone()], dest, 500_000, 0, epoch, 1000)
            .expect("base rate");
        let fast = prepare_payment(&seed, Network::Testnet, &[utxo.clone()], dest, 500_000, 0, epoch, 5000)
            .expect("fast rate");
        assert!(fast.fee > base.fee, "higher rate pays a higher fee");
        assert_eq!(fast.fee, base.fee * 5, "fee scales linearly with the rate");
        assert!(check_fee_rate(0).is_err(), "zero is not a rate");
        assert!(check_fee_rate(99).is_err(), "below relay floor");
        assert!(check_fee_rate(100).is_ok());
        assert!(check_fee_rate(100_000).is_ok());
        assert!(check_fee_rate(100_001).is_err(), "fat-finger guard");
        assert!(prepare_payment(&seed, Network::Testnet, &[utxo], dest, 500_000, 0, epoch, 10).is_err());
    }

    #[test]
    fn prepare_payment_skips_stake_guarded_utxos() {
        const P2PKH: &str = "76a914abababababababababababababababababababab88ac";
        const GUARDED: &str = "2ea22c80207a4029b3230f7943957f81f24a9f438e20a0ede2779bcb12be228be5ab07debd81031210008203000401cc4c90";
        let seed = [9u8; 64];
        let mk = |txid: &str, value: u64, script: &str| ScannedUtxo {
            txid_display: txid.into(),
            vout: 0,
            value,
            script_hex: script.into(),
            address: "mX".into(),
            change: 0,
            index: 0,
            coinbase: false,
            confirmations: 50,
        };
        let dest = "mvSvTtvD9H9fkgi8MGDyLALgRaR2LhnWFM";
        let epoch = lm_keys::sign::Epoch::Sprout;
        // Mixed: 600k P2PKH + 100M guarded funds a 500k send with ~100k
        // change — the guarded giant is untouched.
        let mixed = vec![
            mk(&"aa".repeat(32), 600_000, P2PKH),
            mk(&"bb".repeat(32), 100_000_000, GUARDED),
        ];
        let d = prepare_payment(&seed, Network::Testnet, &mixed, dest, 500_000, 0, epoch, DEFAULT_FEE_RATE_SAT_PER_KB)
            .unwrap_or_else(|e| panic!("mixed funds: {e}"));
        assert!(d.change < 200_000, "change {} uses guarded funds", d.change);
        // Guarded-only: honest error naming the lock.
        let only = vec![mk(&"bb".repeat(32), 100_000_000, GUARDED)];
        match prepare_payment(&seed, Network::Testnet, &only, dest, 1_000, 0, epoch, DEFAULT_FEE_RATE_SAT_PER_KB) {
            Ok(_) => panic!("guarded-only must not fund"),
            Err(e) => assert!(e.contains("stake-guarded"), "error was: {e}"),
        }
    }

    /// Activity decoding against canned chains (no node needed).
    #[test]
    fn row_builder_marks_receive_send_self() {
        let own = "76a914aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa88ac".to_string();
        let chg = "76a914cccccccccccccccccccccccccccccccccccccccc88ac".to_string();
        let ext = "76a914bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb88ac".to_string();
        let mut own_scripts = HashMap::new();
        own_scripts.insert(own.clone(), (0u32, 0u32));
        own_scripts.insert(chg.clone(), (1u32, 0u32));

        // Receive: foreign input → own output.
        let rx = DecodedTx {
            vin: vec![(Some("eeee".into()), Some(0))],
            vouts: vec![(50_000, own.clone(), Some("mOwn".into()))],
            time: 100,
            confirmations: 3,
            height: Some(10),
        };
        let prev = |_: &str, _: u32| -> Option<(u64, String)> {
            Some((
                80_000,
                "76a914dddddddddddddddddddddddddddddddddddddddd88ac".into(),
            ))
        };
        let mut used = Vec::new();
        let row = row_for_tx("rxid", &rx, &prev, &own_scripts, &mut used);
        assert_eq!(
            (row.category.as_str(), row.amount, row.confirmations),
            ("receive", 0.0005, 3)
        );
        assert_eq!(row.address, "mOwn");
        assert_eq!(used, vec![(0, 0)]);

        // Send: own input → external + change outputs.
        let sx = DecodedTx {
            vin: vec![(Some("rxid".into()), Some(0))],
            vouts: vec![
                (30_000, ext.clone(), Some("mExt".into())),
                (19_000, chg.clone(), Some("mChg".into())),
            ],
            time: 200,
            confirmations: 1,
            height: Some(11),
        };
        let prev2 = |_: &str, _: u32| -> Option<(u64, String)> { Some((50_000, own.clone())) };
        let mut used2 = Vec::new();
        let row = row_for_tx("sxid", &sx, &prev2, &own_scripts, &mut used2);
        assert_eq!((row.category.as_str(), row.amount), ("send", -0.0003));
        assert_eq!(row.address, "mExt");
        assert_eq!(fee_of(&sx, &prev2), Some(1_000));
        assert!(used2.contains(&(0, 0)) && used2.contains(&(1, 0)));

        // Self (consolidation): own input, only own outputs.
        let self_tx = DecodedTx {
            vin: vec![(Some("rxid".into()), Some(0))],
            vouts: vec![
                (29_000, chg.clone(), Some("mChg".into())),
                (20_000, own.clone(), Some("mOwn".into())),
            ],
            time: 300,
            confirmations: 2,
            height: Some(12),
        };
        let mut used3 = Vec::new();
        let row = row_for_tx("selfid", &self_tx, &prev2, &own_scripts, &mut used3);
        assert_eq!((row.category.as_str(), row.amount), ("self", 0.0));

        // Unknown prevout → direction falls back to receive, fee unknown.
        let prev_none = |_: &str, _: u32| -> Option<(u64, String)> { None };
        assert_eq!(fee_of(&sx, &prev_none), None);
    }

    /// Live wallet e2e on a throwaway regtest node (explicit runs only):
    /// boots luckyd, funds the wallet's first address, then exercises the
    /// REAL backend paths — scan, balances, activity, local-sign send,
    /// confirm — against chain truth.
    ///
    /// `LUCKYD_BIN=... cargo test -p lucky-lode-wallet -- --ignored`
    /// (takes ~10 min: the in-node CPU miner is slow on LuckyHash).
    #[test]
    #[ignore]
    fn live_regtest_wallet_e2e() {
        use std::process::Command;
        use std::time::{Duration, Instant};

        let luckyd = std::env::var("LUCKYD_BIN")
            .or_else(|_| std::env::var("VERUSD_BIN"))
            .unwrap_or_default();
        let luckyd = if !luckyd.is_empty() {
            luckyd
        } else {
            let d = format!("{}/../../verus/src", env!("CARGO_MANIFEST_DIR"));
            let renamed = format!("{d}/luckyd");
            if std::path::Path::new(&renamed).exists() {
                renamed
            } else {
                format!("{d}/verusd")
            }
        };
        let rpc_port = 18971;
        let p2p_port = 18970;
        let dir = std::env::temp_dir().join(format!("lk-wsnit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("tmpdir");
        let rpc_url = format!("http://127.0.0.1:{rpc_port}");
        let start = Command::new(&luckyd)
            .args([
                "-regtest",
                "-ac_algo=luckyhash",
                &format!("-datadir={}", dir.display()),
                "-rpcuser=u",
                "-rpcpassword=p",
                &format!("-rpcport={rpc_port}"),
                &format!("-port={p2p_port}"),
                "-daemon",
                "-listen=0",
                "-gen=0",
                "-lucky-coinbase-maturity=1",
            ])
            .output()
            .expect("spawn luckyd");
        assert!(start.status.success(), "luckyd failed to start");
        let rpc = RpcClient::from_url(&rpc_url, "u", "p").expect("rpc client");
        // Wait for RPC.
        let t0 = Instant::now();
        loop {
            if rpc.call("getblockcount", json!([])).is_ok() {
                break;
            }
            assert!(
                t0.elapsed() < Duration::from_secs(120),
                "node never came up"
            );
            std::thread::sleep(Duration::from_secs(2));
        }
        let stop_node = || {
            let _ = rpc.call("stop", json!([]));
        };
        let mine = |n: u64| {
            use std::sync::atomic::{AtomicBool, Ordering};
            use std::sync::Arc;
            let t0 = Instant::now();
            let before: u64 = rpc
                .call("getblockcount", json!([]))
                .expect("count")
                .as_u64()
                .expect("u64");
            // The internal LuckyHash miner is slow (minutes/block) and the
            // RPC client's read timeout is 20s, so a single `generate`
            // call can't be relied on: hammer generate from a background
            // thread (the node keeps mining across client disconnects;
            // extra calls during a run are ignored) while we poll height.
            let stop = Arc::new(AtomicBool::new(false));
            let miner = std::thread::spawn({
                let stop = Arc::clone(&stop);
                let url = rpc_url.clone();
                move || {
                    let r = RpcClient::from_url(&url, "u", "p").expect("rpc client");
                    while !stop.load(Ordering::Relaxed) {
                        let _ = r.call("generate", json!([1]));
                        std::thread::sleep(Duration::from_secs(3));
                    }
                }
            });
            loop {
                let h: u64 = rpc
                    .call("getblockcount", json!([]))
                    .expect("count")
                    .as_u64()
                    .expect("u64");
                if h >= before + n {
                    break;
                }
                assert!(
                    t0.elapsed() < Duration::from_secs(900),
                    "mining stalled at height {h}"
                );
                std::thread::sleep(Duration::from_secs(10));
            }
            stop.store(true, Ordering::Relaxed);
            let _ = miner.join();
        };

        let run = || -> Result<(), String> {
            // NOTE: regtest boots mainnet-params here (chain "main") and
            // shares mainnet prefixes, so just follow the node.
            let net = network_of(&rpc)?;
            eprintln!("e2e: chain network = {net:?}");
            mine(2);
            eprintln!("e2e: mined 2");
            // Fresh wallet in the throwaway dir.
            let wdir = dir.join("walletapp");
            std::fs::create_dir_all(&wdir).map_err(|e| e.to_string())?;
            assert!(!has_any_wallet(&wdir));
            create_wallet_enc(&wdir, "e2e test password")?;
            assert!(has_any_wallet(&wdir));
            let (seed, _) = unlock_seed(&wdir, "e2e test password")?;
            let addr0 = current_receive(&seed, net, 0)?;
            eprintln!("e2e: wallet address {addr0}");
            // Fund it from the node (regtest coins have no value).
            let fund: String = rpc
                .call("sendtoaddress", json!([addr0, "0.01"]))
                .map_err(|e| format!("fund: {e}"))?
                .as_str()
                .ok_or("fund txid")?
                .to_string();
            eprintln!("e2e: funded {fund}, waiting 1 conf");
            mine(1);
            // Balances see it.
            let scan_res = scan(&rpc, &seed, net, 0, 0, &wdir)?;
            assert!(!scan_res.utxos.is_empty(), "funding UTXO missing");
            let total: u64 = scan_res.utxos.iter().map(|u| u.value).sum();
            assert_eq!(total, 1_000_000);
            let b = balances_of(&scan_res, 3);
            assert_eq!((b.available, b.immature), (1_000_000, 0));
            // Activity shows the receive.
            let map = own_script_map(&seed, net, 0, 0)?;
            let page = activity(&rpc, &scan_res.txids, &map, 50, scan_res.tip, &wdir)?;
            assert!(page.rows.iter().any(|r| r.category == "receive"
                && (r.amount - 0.01).abs() < 1e-9
                && r.txid == fund));
            eprintln!("e2e: balances + activity OK");
            // Spend half back to a node address, signed locally.
            let dest: String = rpc
                .call("getnewaddress", json!([]))
                .map_err(|e| format!("dest: {e}"))?
                .as_str()
                .ok_or("dest addr")?
                .to_string();
            let r = send_payment(&rpc, &seed, net, &scan_res.utxos, &dest, 400_000, 0, DEFAULT_FEE_RATE_SAT_PER_KB)?;
            assert_eq!(r.change_used, Some(0));
            assert!(r.fee > 0 && r.fee < 10_000);
            eprintln!("e2e: sent {} (fee {}), waiting 1 conf", r.txid, r.fee);
            mine(1);
            // Destination got exactly 400k; change is back in the wallet.
            let got: f64 = rpc
                .call("getreceivedbyaddress", json!([dest, 1]))
                .map_err(|e| format!("receipt: {e}"))?
                .as_f64()
                .ok_or("receipt amount")?;
            assert!((got - 0.004).abs() < 1e-9, "got {got}");
            let scan_res2 = scan(&rpc, &seed, net, 0, 1, &wdir)?;
            let b2 = balances_of(&scan_res2, 5);
            assert_eq!(b2.available, 1_000_000 - 400_000 - r.fee);
            // Detail renders with fee.
            let map2 = own_script_map(&seed, net, 0, 1)?;
            let d = tx_detail(&rpc, &r.txid, &map2, &wdir)?;
            assert_eq!(
                d.get("fee").and_then(|v| v.as_f64()).unwrap_or(-1.0),
                r.fee as f64 / 100_000_000.0
            );
            // Restore path rediscovers the used addresses.
            let (rn, cn, used) = full_scan_used(&rpc, &seed, net)?;
            assert!(used >= 2, "used={used}");
            assert!(rn >= 1 && cn >= 1, "cursors {rn}/{cn}");
            eprintln!("e2e: DONE");
            Ok(())
        };
        let out = run();
        stop_node();
        let _ = std::fs::remove_dir_all(&dir);
        out.expect("wallet e2e");
    }

    /// Live TESTNET round-trip against the real public node (explicit runs
    /// only): fresh temp wallet → funded from the node wallet → scanned →
    /// local-signed partial send back. Tiny amounts.
    ///
    /// `LUCKYLODE_RPC_URL=https://luckylode.cc/node/ LUCKYLODE_RPC_USER=u \
    ///  LUCKYLODE_RPC_PASS=p cargo test live_testnet_wallet_roundtrip \
    ///  -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn live_testnet_wallet_roundtrip() {
        use std::time::{Duration, Instant};
        let url = match std::env::var("LUCKYLODE_RPC_URL") {
            Ok(u) => u,
            Err(_) => return, // silent skip, like the lm-stratum live tests
        };
        let user = std::env::var("LUCKYLODE_RPC_USER").unwrap_or_default();
        let pass = std::env::var("LUCKYLODE_RPC_PASS").unwrap_or_default();
        let rpc = RpcClient::from_url(&url, &user, &pass).expect("rpc");
        let net = network_of(&rpc).expect("chain");
        assert_eq!(net, Network::Testnet);
        let dir = std::env::temp_dir().join(format!("lk-wlive-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("tmpdir");
        create_wallet_enc(&dir, "live test password").expect("wallet");
        let (seed, _) = unlock_seed(&dir, "live test password").expect("seed");
        let addr = current_receive(&seed, net, 0).expect("addr");
        eprintln!("live wallet address: {addr}");
        // Fund from the node wallet (the pool runs one; testnet coins).
        let fund: String = rpc
            .call("sendtoaddress", json!([addr, "0.01"]))
            .expect("fund")
            .as_str()
            .expect("txid")
            .to_string();
        eprintln!("funded: {fund}");
        // Wait for 1 confirmation (blocks ~1/min on the tuned testnet).
        let t0 = Instant::now();
        loop {
            let confs = rpc
                .call("getrawtransaction", json!([fund, 1]))
                .ok()
                .and_then(|v| v.get("confirmations").and_then(|c| c.as_u64()))
                .unwrap_or(0);
            if confs >= 1 {
                break;
            }
            assert!(
                t0.elapsed() < Duration::from_secs(900),
                "funding never confirmed"
            );
            std::thread::sleep(Duration::from_secs(10));
        }
        // Balances/activity see it.
        let scan = scan(&rpc, &seed, net, 0, 0, &dir).expect("scan");
        let b = balances_of(&scan, 0);
        assert!(b.available >= 1_000_000, "available {}", b.available);
        let map = own_script_map(&seed, net, 0, 0).expect("map");
        let page = activity(&rpc, &scan.txids, &map, 20, scan.tip, &dir).expect("activity");
        assert!(page
            .rows
            .iter()
            .any(|r| r.txid == fund && r.category == "receive"));
        // Send part of it back to the pool node address.
        let dest = "n2dQr43hTDtDx5XwKnwozN7un4ZbpFzESc";
        let r = send_payment(&rpc, &seed, net, &scan.utxos, dest, 400_000, 0, DEFAULT_FEE_RATE_SAT_PER_KB).expect("send");
        eprintln!("sent back {} (fee {})", r.txid, r.fee);
        // Confirm on chain.
        let t0 = Instant::now();
        loop {
            let confs = rpc
                .call("getrawtransaction", json!([r.txid, 1]))
                .ok()
                .and_then(|v| v.get("confirmations").and_then(|c| c.as_u64()))
                .unwrap_or(0);
            if confs >= 1 {
                break;
            }
            assert!(
                t0.elapsed() < Duration::from_secs(900),
                "spend never confirmed"
            );
            std::thread::sleep(Duration::from_secs(10));
        }
        let _ = std::fs::remove_dir_all(&dir);
        eprintln!("live testnet wallet round-trip OK");
    }

    /// Encrypted vault behavior (no node needed).
    #[test]
    fn vault_roundtrip_wrong_password_and_tamper() {
        let phrase = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
        let blob = encrypt_phrase(phrase, "correct horse 123").expect("seal");
        assert!(blob.starts_with(b"LKW1"));
        assert_eq!(
            decrypt_phrase(&blob, "correct horse 123").expect("open"),
            phrase
        );
        // Wrong password fails closed (auth tag mismatch, no oracle).
        assert!(decrypt_phrase(&blob, "wrong password 123").is_err());
        // Flipped ciphertext byte fails closed.
        let mut bad = blob.clone();
        let n = bad.len();
        bad[n - 1] ^= 1;
        assert!(decrypt_phrase(&bad, "correct horse 123").is_err());
        // Truncated / wrong magic rejected.
        assert!(decrypt_phrase(&blob[..10], "correct horse 123").is_err());
        assert!(decrypt_phrase(b"NOPE rest of bytes here........", "correct horse 123").is_err());
        // Weak passwords rejected at creation time.
        assert!(encrypt_phrase(phrase, "short").is_err());
    }

    #[test]
    fn vault_create_migrate_change_password() {
        let dir = std::env::temp_dir().join(format!("lk-wvlt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("tmpdir");
        assert!(!has_any_wallet(&dir));
        // Legacy path first (what pre-encryption installs have): a raw
        // 0600 phrase file, exactly as the old backend wrote it.
        let phrase = SeedPhrase::generate().expect("phrase").phrase();
        write_0600(&seed_path(&dir), phrase.as_bytes()).expect("legacy file");
        assert!(is_legacy_plaintext(&dir));
        // Migrate seals it and shreds the plaintext.
        let back = migrate_plaintext(&dir, "migrate pw 1").expect("migrate");
        assert_eq!(back, phrase);
        assert!(!is_legacy_plaintext(&dir));
        assert!(!seed_path(&dir).is_file());
        assert!(enc_path(&dir).is_file());
        let (seed, _) = unlock_seed(&dir, "migrate pw 1").expect("unlock");
        assert_eq!(seed, SeedPhrase::parse(&phrase).expect("parse").seed());
        // Rotate the password: new works, old stops.
        change_password(&dir, "migrate pw 1", "rotated pw 22").expect("rotate");
        assert!(unlock_seed(&dir, "migrate pw 1").is_err());
        unlock_seed(&dir, "rotated pw 22").expect("unlock new");
        // Second create refuses (restore replaces explicitly).
        assert!(create_wallet_enc(&dir, "another pw 33").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn session_expiry_and_wipe() {
        use std::time::{Duration, Instant};
        let mut s = Session::new([7u8; 64], "word ".repeat(12).trim().to_string());
        assert!(!s.expired());
        s.last_active = Instant::now() - Duration::from_secs(IDLE_LOCK_SECS + 1);
        assert!(s.expired());
        s.wipe();
        assert_eq!(s.seed, [0u8; 64]);
        assert!(s.phrase.is_empty());
    }

    /// Dev helper for GUI smoke tests: pre-seed an encrypted vault in a
    /// throwaway config dir so the lock screen can be exercised.
    /// `LKW_TEST_DIR=<dir> LKW_TEST_PW=<pw> cargo test make_vault_helper -- --ignored`
    #[test]
    #[ignore]
    fn make_vault_helper() {
        let dir = std::path::PathBuf::from(std::env::var("LKW_TEST_DIR").expect("LKW_TEST_DIR"));
        let pw = std::env::var("LKW_TEST_PW").expect("LKW_TEST_PW");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let phrase = create_wallet_enc(&dir, &pw).expect("create vault");
        eprintln!("vault at {dir:?} phrase: {phrase}");
    }
}
