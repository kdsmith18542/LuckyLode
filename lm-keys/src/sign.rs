//! Transparent signing. Sprout-era legacy `SIGHASH_ALL` is live on
//! testnet/regtest today; the ZIP-143/243 path (Blake2b, branch-id
//! personalized, `zip243.rs`) activates with the overwinter/sapling
//! epochs and is selected via [`Epoch`] — the caller reads the epoch off
//! the connected node (testnet: overwinter @ 207500, sapling after).
//!
//! Legacy layout mirrors `CTransactionSignatureSerializer` + the sprout
//! leg of `SignatureHash()` (interpreter.cpp): the spending input's
//! scriptSig is replaced by the prevout scriptPubKey (scriptCode),
//! `SIGHASH_ALL` is appended LE32, double-SHA256. Signatures are low-S
//! normalized (standard relay rule).

use secp256k1::ecdsa::{RecoverableSignature, RecoveryId, Signature};
use secp256k1::{Message, PublicKey, Secp256k1, SecretKey};

use crate::address::sha256d;
use crate::tx::{
    put_bytes, put_compactsize, put_i64, put_u32, txid_display, OutPoint, Transaction, TxIn, TxOut,
    SEQUENCE_FINAL, SIGHASH_ALL,
};
pub use crate::zip243::{Epoch, SAPLING_BRANCH_ID};

// ---- Verus-compatible signed messages (node `verifymessage` RPC) --------
//
// Mirrors `signmessage` for t-addresses in rpcwallet.cpp byte-for-byte:
//   h1 = SHA256(compactsize(len(msg)) || msg)          (single SHA256)
//   h2 = SHA256(compactsize(len(prefix)) || prefix || h1)
//   sig = compact recoverable ECDSA over h2, base64.
// The prefix is the consensus-frozen `verusDataSignaturePrefix`
// ("Verus signed data:\n", main.cpp). Do NOT "fix" this to the Bitcoin
// magic — the node would reject it.

pub const VERUS_SIGNED_DATA_PREFIX: &[u8] = b"Verus signed data:\n";

fn sha256(data: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    Sha256::digest(data).into()
}

fn compact_size(b: &mut Vec<u8>, n: u64) {
    if n < 253 {
        b.push(n as u8);
    } else {
        // Only short canonical strings are used; full compactsize for safety.
        b.push(253);
        b.extend_from_slice(&(n as u16).to_le_bytes());
    }
}

fn b64_encode(b: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(b.len().div_ceil(3) * 4);
    for chunk in b.chunks(3) {
        let n = ((chunk[0] as u32) << 16)
            | ((*chunk.get(1).unwrap_or(&0)) as u32) << 8
            | (*chunk.get(2).unwrap_or(&0)) as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(T[(n >> 6) as usize & 63] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(T[n as usize & 63] as char);
        } else {
            out.push('=');
        }
    }
    out
}

fn b64_decode(s: &str) -> Result<Vec<u8>, String> {
    let val = |c: u8| -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    };
    let mut out = Vec::new();
    for chunk in s.as_bytes().chunks(4) {
        let mut buf = [0u8; 4];
        let mut n = 0usize;
        for (i, c) in chunk.iter().enumerate() {
            if *c == b'=' {
                break;
            }
            buf[i] = val(*c).ok_or("invalid base64")?;
            n = i + 1;
        }
        match n {
            2 => out.push((buf[0] << 2) | (buf[1] >> 4)),
            3 => {
                out.push((buf[0] << 2) | (buf[1] >> 4));
                out.push((buf[1] << 4) | (buf[2] >> 2));
            }
            4 => {
                out.push((buf[0] << 2) | (buf[1] >> 4));
                out.push((buf[1] << 4) | (buf[2] >> 2));
                out.push((buf[2] << 6) | buf[3]);
            }
            _ => return Err("invalid base64 length".to_string()),
        }
    }
    Ok(out)
}

/// Message hash exactly as the node's `signmessage` builds it (h1).
fn signed_msg_hash(message: &str) -> [u8; 32] {
    let mut m = Vec::with_capacity(message.len() + 8);
    compact_size(&mut m, message.len() as u64);
    m.extend_from_slice(message.as_bytes());
    sha256(&m)
}

/// The full pre-image hash the node signs (h2): prefix || h1.
fn signed_data_hash(message: &str) -> [u8; 32] {
    let h1 = signed_msg_hash(message);
    let mut p = Vec::with_capacity(h1.len() + VERUS_SIGNED_DATA_PREFIX.len() + 8);
    compact_size(&mut p, VERUS_SIGNED_DATA_PREFIX.len() as u64);
    p.extend_from_slice(VERUS_SIGNED_DATA_PREFIX);
    p.extend_from_slice(&h1);
    sha256(&p)
}

/// Sign `message` the way the node's `signmessage` does, returning the
/// base64 compact signature that `verifymessage` accepts.
pub fn sign_message(secret_32: &[u8; 32], message: &str) -> Result<String, String> {
    let secp = Secp256k1::new();
    let sk = SecretKey::from_slice(secret_32).map_err(|e| format!("secret: {e}"))?;
    let h2 = signed_data_hash(message);
    let sig = secp.sign_ecdsa_recoverable(&Message::from_digest(h2), &sk);
    let (recid, sigb) = sig.serialize_compact();
    let mut out = Vec::with_capacity(65);
    // +4: all wallet keys are compressed (`CKey::SignCompact` convention).
    out.push(31u8.wrapping_add(recid.to_i32() as u8));
    out.extend_from_slice(&sigb);
    Ok(b64_encode(&out))
}

/// Verify a base64 compact signature against a message, returning the
/// compressed pubkey it proves control of.
pub fn verify_message(sig_b64: &str, message: &str) -> Result<[u8; 33], String> {
    let sig = b64_decode(sig_b64)?;
    if sig.len() != 65 {
        return Err("signature must be 65 bytes".to_string());
    }
    if sig[0] < 27 || sig[0] > 34 {
        return Err("bad recovery prefix".to_string());
    }
    let recid = RecoveryId::from_i32(i32::from(sig[0] - 27) & 3)
        .map_err(|e| format!("recovery id: {e}"))?;
    let rs =
        RecoverableSignature::from_compact(&sig[1..], recid).map_err(|e| format!("sig: {e}"))?;
    let h2 = signed_data_hash(message);
    let pk = Secp256k1::new()
        .recover_ecdsa(&Message::from_digest(h2), &rs)
        .map_err(|_| "bad signature for message".to_string())?;
    Ok(pk.serialize())
}

/// Legacy `SIGHASH_ALL` digest for `input`, committing to `script_code`
/// (the prevout's scriptPubKey).
pub fn legacy_sighash(
    tx: &Transaction,
    input: usize,
    script_code: &[u8],
) -> Result<[u8; 32], String> {
    if tx.overwintered {
        return Err("legacy sighash needs a sprout-era tx".to_string());
    }
    if input >= tx.vin.len() {
        return Err("input out of range".to_string());
    }
    let mut b = Vec::new();
    put_u32(&mut b, tx.version as u32);
    put_compactsize(&mut b, tx.vin.len() as u64);
    for (i, inp) in tx.vin.iter().enumerate() {
        b.extend_from_slice(&inp.prevout.txid);
        put_u32(&mut b, inp.prevout.vout);
        if i == input {
            put_bytes(&mut b, script_code);
        } else {
            put_compactsize(&mut b, 0);
        }
        put_u32(&mut b, inp.sequence);
    }
    put_compactsize(&mut b, tx.vout.len() as u64);
    for o in &tx.vout {
        put_i64(&mut b, o.value);
        put_bytes(&mut b, &o.script_pubkey);
    }
    put_u32(&mut b, tx.locktime);
    put_u32(&mut b, SIGHASH_ALL);
    Ok(sha256d(&b))
}

fn is_p2pkh(script: &[u8]) -> bool {
    script.len() == 25
        && script[0] == 0x76
        && script[1] == 0xa9
        && script[2] == 0x14
        && script[23] == 0x88
        && script[24] == 0xac
}

fn is_p2pk(script: &[u8]) -> bool {
    script.len() == 35 && script[0] == 0x21 && script[34] == 0xac
}

fn push(b: &mut Vec<u8>, data: &[u8]) {
    assert!(data.len() < 76, "push too large for single opcode");
    b.push(data.len() as u8);
    b.extend_from_slice(data);
}

/// Sign `input` (scriptCode = prevout script). Returns the scriptSig:
/// `<sig+01> <pubkey>` for P2PKH, `<sig+01>` for bare P2PK (coinbases).
/// `epoch` picks the signature-hash leg and must match the tx format
/// (the builder constructs the tx for the epoch); `amount` is the spent
/// output's value (ZIP-243 hashes it; the legacy leg ignores it).
pub fn sign_input(
    tx: &Transaction,
    input: usize,
    script_code: &[u8],
    secret: &[u8; 32],
    amount: u64,
    epoch: Epoch,
) -> Result<Vec<u8>, String> {
    let secp = Secp256k1::new();
    let sk = SecretKey::from_slice(secret).map_err(|e| format!("secret: {e}"))?;
    let sighash = match (epoch, tx.overwintered, tx.version) {
        (Epoch::Sprout, false, _) => legacy_sighash(tx, input, script_code)?,
        (Epoch::Sprout, true, _) => {
            return Err("sprout epoch but the tx is overwintered (format mismatch)".to_string());
        }
        (epoch, true, v) => {
            let want = match epoch {
                Epoch::Overwinter(_) => crate::tx::OVERWINTER_TX_VERSION,
                Epoch::Sapling(_) => crate::tx::SAPLING_TX_VERSION,
                Epoch::Sprout => unreachable!("sprout handled above"),
            };
            if v != want {
                return Err(format!(
                    "epoch/tx format mismatch: epoch wants tx v{want}, tx is v{v}"
                ));
            }
            let branch = epoch
                .branch_id()
                .expect("overwintered epochs carry a branch id");
            crate::zip243::sighash(tx, input, script_code, amount as i64, SIGHASH_ALL, branch)?
        }
        (_, false, _) => {
            return Err(
                "overwintered epoch but the tx is sprout-era (format mismatch)".to_string(),
            );
        }
    };
    let mut sig = secp.sign_ecdsa(&Message::from_digest(sighash), &sk);
    sig.normalize_s();
    let mut der = sig.serialize_der().to_vec();
    der.push(SIGHASH_ALL as u8);
    let pk = PublicKey::from_secret_key(&secp, &sk).serialize();
    let mut script_sig = Vec::new();
    if is_p2pkh(script_code) {
        push(&mut script_sig, &der);
        push(&mut script_sig, &pk);
    } else if is_p2pk(script_code) {
        push(&mut script_sig, &der);
    } else {
        return Err("unsupported prevout script (only P2PKH/P2PK)".to_string());
    }
    Ok(script_sig)
}

/// Verify a DER(+hashtype) signature against a sighash + pubkey.
pub fn verify_sig(
    sighash: &[u8; 32],
    der_with_hashtype: &[u8],
    pubkey: &[u8; 33],
) -> Result<bool, String> {
    if der_with_hashtype.last() != Some(&(SIGHASH_ALL as u8)) {
        return Err("only SIGHASH_ALL supported".to_string());
    }
    let secp = Secp256k1::new();
    let sig = Signature::from_der(&der_with_hashtype[..der_with_hashtype.len() - 1])
        .map_err(|e| format!("der: {e}"))?;
    let pk = PublicKey::from_slice(pubkey).map_err(|e| format!("pubkey: {e}"))?;
    Ok(secp
        .verify_ecdsa(&Message::from_digest(*sighash), &sig, &pk)
        .is_ok())
}

/// A spendable output with its key (from `getaddressutxos` + the keychain).
#[derive(Debug, Clone)]
pub struct SpendUtxo {
    pub txid: [u8; 32], // wire order (see tx::parse_txid_display)
    pub vout: u32,
    pub value: u64, // satoshis
    pub script: Vec<u8>,
    pub secret: [u8; 32],
}

pub const DEFAULT_FEE_RATE_SAT_PER_KB: u64 = 1000;
/// Outputs below this become fee (the node is the final dust arbiter).
pub const DUST_SATS: u64 = 1000;

#[derive(Debug, Clone)]
pub struct BuiltSpend {
    pub hex: String,
    pub txid: String, // display order
    pub fee: u64,
    pub inputs: usize,
    pub change: u64,
}

/// Build + sign a transparent spend in `epoch`'s format: `amount` to
/// `dest_script`, change to `change_script`. UTXOs are taken in order
/// until covered.
pub fn build_spend(
    utxos: &[SpendUtxo],
    dest_script: &[u8],
    amount: u64,
    change_script: &[u8],
    fee_rate_sat_per_kb: u64,
    epoch: Epoch,
) -> Result<BuiltSpend, String> {
    if amount < DUST_SATS {
        return Err(format!("dust output: {amount}"));
    }
    if utxos.is_empty() {
        return Err("no utxos".to_string());
    }
    let fee_for = |n_in: usize, n_out: usize| -> u64 {
        let vbytes = 10 + 148 * n_in as u64 + 34 * n_out as u64;
        vbytes.saturating_mul(fee_rate_sat_per_kb).div_ceil(1000)
    };
    // Select inputs (assume a change output for the estimate).
    let mut total = 0u64;
    let mut n = 0usize;
    for u in utxos {
        total += u.value;
        n += 1;
        if total >= amount + fee_for(n, 2) {
            break;
        }
    }
    let mut outs = 2;
    let mut fee = fee_for(n, outs);
    if total < amount + fee {
        return Err(format!(
            "insufficient funds: have {total}, need {}",
            amount + fee
        ));
    }
    let mut change = total - amount - fee;
    if change < DUST_SATS {
        // No change output; the dust becomes fee.
        outs = 1;
        fee = total - amount;
        change = 0;
    }
    let inputs: Vec<TxIn> = utxos[..n]
        .iter()
        .map(|u| TxIn {
            prevout: OutPoint {
                txid: u.txid,
                vout: u.vout,
            },
            script_sig: Vec::new(),
            sequence: SEQUENCE_FINAL,
        })
        .collect();
    let mut out_v = vec![TxOut {
        value: amount as i64,
        script_pubkey: dest_script.to_vec(),
    }];
    if outs == 2 {
        out_v.push(TxOut {
            value: change as i64,
            script_pubkey: change_script.to_vec(),
        });
    }
    let mut tx = match epoch {
        Epoch::Sprout => Transaction::legacy(inputs, out_v.clone()),
        Epoch::Overwinter(_) => Transaction::overwinter_v3(inputs, out_v.clone()),
        Epoch::Sapling(_) => Transaction::sapling_v4(inputs, out_v),
    };
    for (i, u) in utxos[..n].iter().enumerate() {
        tx.vin[i].script_sig = sign_input(&tx, i, &u.script, &u.secret, u.value, epoch)?;
    }
    let raw = tx.serialize();
    Ok(BuiltSpend {
        hex: hex::encode(&raw),
        txid: txid_display(&tx.txid()),
        fee,
        inputs: n,
        change,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::address::Keypair;
    use crate::chain::Network;
    use crate::tx::parse_txid_display;

    /// KAT: a real Bitcoin mainnet P2PKH spend. The fork's sprout-era
    /// sighash is byte-identical to Bitcoin's, so a mainnet signature must
    /// verify against our recomputed digest.
    #[test]
    fn bitcoin_mainnet_p2pkh_spend_verifies() {
        let kat: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/btc_p2pkh_spend.json"))
                .expect("fixture");
        let raw = hex::decode(kat["hex"].as_str().expect("hex")).expect("hex");
        let (tx, used) = Transaction::deserialize(&raw).expect("decode");
        assert_eq!(used, raw.len());
        // Re-serialization is byte-identical (decoder understands the wire).
        assert_eq!(
            hex::encode(tx.serialize()),
            kat["hex"].as_str().expect("hex")
        );
        assert_eq!(
            txid_display(&tx.txid()),
            kat["txid"].as_str().expect("txid")
        );
        assert_eq!(tx.vin.len(), 1);
        let vin0 = &kat["vin0"];
        assert_eq!(
            txid_display(&tx.vin[0].prevout.txid),
            vin0["txid"].as_str().expect("prev txid")
        );
        assert_eq!(
            tx.vin[0].prevout.vout,
            vin0["vout"].as_u64().expect("vout") as u32
        );
        assert_eq!(
            hex::encode(&tx.vin[0].script_sig),
            vin0["scriptsig"].as_str().expect("scriptsig")
        );
        // Split scriptSig: <sig+01> <pubkey>.
        let ss = &tx.vin[0].script_sig;
        assert_eq!(ss[0] as usize, ss.len() - 2 - 33);
        let sig = &ss[1..1 + ss[0] as usize];
        assert_eq!(ss[1 + ss[0] as usize], 33);
        let pk: &[u8; 33] = ss[2 + ss[0] as usize..].try_into().expect("33-byte pubkey");
        let script_code =
            hex::decode(vin0["prevout_script"].as_str().expect("script")).expect("script");
        let sighash = legacy_sighash(&tx, 0, &script_code).expect("sighash");
        assert!(verify_sig(&sighash, sig, pk).expect("verify"));
    }

    #[test]
    fn legacy_sign_verify_roundtrip_and_tamper_fails() {
        let k = Keypair::from_secret([7u8; 32]).expect("key");
        let script = k.p2pkh_script(Network::Testnet);
        let tx = Transaction::legacy(
            vec![TxIn {
                prevout: OutPoint {
                    txid: [0xabu8; 32],
                    vout: 3,
                },
                script_sig: Vec::new(),
                sequence: SEQUENCE_FINAL,
            }],
            vec![TxOut {
                value: 50_000,
                script_pubkey: script.clone(),
            }],
        );
        let ss = sign_input(&tx, 0, &script, k.secret(), 0, Epoch::Sprout).expect("sign");
        let sighash = legacy_sighash(&tx, 0, &script).expect("sighash");
        let sig_len = ss[0] as usize;
        let pk: &[u8; 33] = ss[1 + sig_len + 1..].try_into().expect("pubkey");
        assert!(verify_sig(&sighash, &ss[1..1 + sig_len], pk).expect("verify"));
        // A tampered digest must not verify.
        let mut bad = sighash;
        bad[0] ^= 1;
        assert!(!verify_sig(&bad, &ss[1..1 + sig_len], pk).expect("verify"));
    }

    #[test]
    fn builder_pays_change_and_fee_deterministically() {
        let ka = Keypair::from_secret([7u8; 32]).expect("key");
        let kb = Keypair::from_secret([9u8; 32]).expect("key");
        let utxos = vec![
            SpendUtxo {
                txid: [0x11u8; 32],
                vout: 0,
                value: 100_000,
                script: ka.p2pkh_script(Network::Testnet),
                secret: *ka.secret(),
            },
            SpendUtxo {
                txid: [0x22u8; 32],
                vout: 1,
                value: 200_000,
                script: ka.p2pkh_script(Network::Testnet),
                secret: *ka.secret(),
            },
        ];
        let dest = kb.p2pkh_script(Network::Testnet);
        let change = ka.p2pkh_script(Network::Testnet);
        let s1 = build_spend(&utxos, &dest, 150_000, &change, 1000, Epoch::Sprout).expect("build");
        let s2 = build_spend(&utxos, &dest, 150_000, &change, 1000, Epoch::Sprout).expect("build");
        assert_eq!(s1.hex, s2.hex, "RFC6979: deterministic");
        assert_eq!(s1.inputs, 2);
        // vsize = 10 + 148*2 + 34*2 = 374 → fee ceil(374*1000/1000) = 374.
        assert_eq!(s1.fee, 374);
        assert_eq!(s1.change, 300_000 - 150_000 - 374);
        // Every input verifies against the built tx.
        let (tx, _) =
            Transaction::deserialize(&hex::decode(&s1.hex).expect("hex")).expect("decode own tx");
        assert_eq!(tx.vin.len(), 2);
        for (i, u) in utxos[..2].iter().enumerate() {
            let h = legacy_sighash(&tx, i, &u.script).expect("sighash");
            let ss = &tx.vin[i].script_sig;
            let sig_len = ss[0] as usize;
            let pk: &[u8; 33] = ss[1 + sig_len + 1..].try_into().expect("pubkey");
            assert!(verify_sig(&h, &ss[1..1 + sig_len], pk).expect("verify"));
        }
    }

    #[test]
    fn builder_rejects_dust_and_insufficient() {
        let ka = Keypair::from_secret([7u8; 32]).expect("key");
        let utxos = vec![SpendUtxo {
            txid: [0x11u8; 32],
            vout: 0,
            value: 100_000,
            script: ka.p2pkh_script(Network::Testnet),
            secret: *ka.secret(),
        }];
        let dest = ka.p2pkh_script(Network::Testnet);
        assert!(build_spend(&utxos, &dest, 500, &dest, 1000, Epoch::Sprout).is_err());
        assert!(build_spend(&utxos, &dest, 99_999_999, &dest, 1000, Epoch::Sprout).is_err());
        assert!(build_spend(&[], &dest, 1000, &dest, 1000, Epoch::Sprout).is_err());
    }

    #[test]
    fn builder_signs_p2pk_coinbase_style_inputs() {
        // Coinbase outputs pay P2PK (`21{33}ac`); spending needs sig-only.
        let ka = Keypair::from_secret([7u8; 32]).expect("key");
        let kb = Keypair::from_secret([9u8; 32]).expect("key");
        let utxos = vec![SpendUtxo {
            txid: parse_txid_display(
                "661a3a91c777eaf7c50bf6afcb20ca168228a2868c426b486d97253d729f5a18",
            )
            .expect("txid"),
            vout: 0,
            value: 150_794,
            script: ka.p2pk_script(),
            secret: *ka.secret(),
        }];
        let s = build_spend(
            &utxos,
            &kb.p2pkh_script(Network::Testnet),
            100_000,
            &ka.p2pkh_script(Network::Testnet),
            1000,
            Epoch::Sprout,
        )
        .expect("build");
        let (tx, _) = Transaction::deserialize(&hex::decode(&s.hex).expect("hex")).expect("decode");
        // P2PK scriptSig is a single push (no pubkey).
        assert_eq!(
            tx.vin[0].script_sig.len(),
            1 + tx.vin[0].script_sig[0] as usize
        );
        let h = legacy_sighash(&tx, 0, &utxos[0].script).expect("sighash");
        let ss = &tx.vin[0].script_sig;
        assert!(verify_sig(&h, &ss[1..], ka.pubkey()).expect("verify"));
    }

    #[test]
    fn verus_signed_message_roundtrip_matches_address() {
        let kp = Keypair::from_secret([5u8; 32]).expect("keypair");
        let net = Network::Testnet;
        let msg = "luckynode-register:v1:abc123:addrXYZ:1792195725";
        let sig = sign_message(kp.secret(), msg).expect("sign");
        let pk = verify_message(&sig, msg).expect("verify");
        assert_eq!(pk, *kp.pubkey(), "recovered pubkey must be the signer's");
        // The pubkey maps to the P2PKH address the node derives.
        assert_eq!(
            Keypair::from_secret(*kp.secret())
                .expect("kp")
                .p2pkh_address(net),
            kp.p2pkh_address(net)
        );
    }

    #[test]
    fn verus_signed_message_rejects_tamper() {
        let kp = Keypair::from_secret([6u8; 32]).expect("keypair");
        let msg = "luckynode-register:v1:abc123:addrXYZ:1792195725";
        let sig = sign_message(kp.secret(), msg).expect("sign");
        // Wrong message → recovery either fails or yields a pubkey that is
        // NOT the signer's (never accept a signature over a different msg).
        if let Ok(pk) = verify_message(&sig, "luckynode-register:v1:abc123:addrXYZ:1792195726") {
            assert_ne!(pk, *kp.pubkey());
        }
        // Wrong key (different secret) → different recovered pubkey.
        let other = Keypair::from_secret([7u8; 32]).expect("keypair");
        assert_ne!(verify_message(&sig, msg).expect("verify"), *other.pubkey());
    }
}
