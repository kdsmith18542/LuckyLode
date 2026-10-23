//! Keys, hashes, base58check, and transparent scripts.
//!
//! All formats match the fork's C++ exactly:
//! - HASH160 = RIPEMD160(SHA256(x)) (`CHash160`)
//! - base58check = version || payload || SHA256D(...)[0..4] (`EncodeBase58Check`)
//! - WIF always carries the compression flag (`CBitcoinSecret`)
//! - P2PKH script `76a914{20}88ac`, P2PK script `21{33}ac`

use secp256k1::{PublicKey, Secp256k1, SecretKey};
use sha2::{Digest, Sha256};

use crate::chain::Network;

/// SHA256D (double SHA256), the checksum and txid hash.
pub fn sha256d(data: &[u8]) -> [u8; 32] {
    let h1 = Sha256::digest(data);
    Sha256::digest(h1).into()
}

/// HASH160 = RIPEMD160(SHA256(x)).
pub fn hash160(data: &[u8]) -> [u8; 20] {
    use ripemd::Ripemd160;
    let h = Ripemd160::digest(Sha256::digest(data));
    h.into()
}

/// base58check encode: version || payload || checksum.
pub fn base58check_encode(version: u8, payload: &[u8]) -> String {
    let mut v = Vec::with_capacity(1 + payload.len() + 4);
    v.push(version);
    v.extend_from_slice(payload);
    base58_with_checksum(&v)
}

/// base58 encode with a SHA256D checksum appended (for payloads like
/// extended keys that already carry their version bytes).
pub fn base58_with_checksum(data: &[u8]) -> String {
    let mut v = Vec::with_capacity(data.len() + 4);
    v.extend_from_slice(data);
    let ck = sha256d(data);
    v.extend_from_slice(&ck[..4]);
    bs58::encode(v).into_string()
}

/// base58check decode: verifies length + checksum, returns (version, payload).
pub fn base58check_decode(s: &str) -> Result<(u8, Vec<u8>), String> {
    let raw = bs58::decode(s)
        .into_vec()
        .map_err(|e| format!("base58: {e}"))?;
    if raw.len() < 5 {
        return Err(format!("base58check too short: {s}"));
    }
    let (body, ck) = raw.split_at(raw.len() - 4);
    if sha256d(body)[..4] != ck[..] {
        return Err(format!("base58check checksum mismatch: {s}"));
    }
    Ok((body[0], body[1..].to_vec()))
}

/// A secp256k1 keypair (always compressed public keys, like the node).
#[derive(Debug, Clone)]
pub struct Keypair {
    secret: [u8; 32],
    pubkey: [u8; 33],
}

impl Keypair {
    pub fn from_secret(secret: [u8; 32]) -> Result<Self, String> {
        let secp = Secp256k1::new();
        let sk = SecretKey::from_slice(&secret).map_err(|e| format!("secret key: {e}"))?;
        let pk = PublicKey::from_secret_key(&secp, &sk);
        Ok(Self {
            secret,
            pubkey: pk.serialize(),
        })
    }

    pub fn secret(&self) -> &[u8; 32] {
        &self.secret
    }

    pub fn pubkey(&self) -> &[u8; 33] {
        &self.pubkey
    }

    /// P2PKH address for a network (`getnewaddress` format).
    pub fn p2pkh_address(&self, net: Network) -> String {
        base58check_encode(net.p2pkh_version(), &hash160(&self.pubkey))
    }

    /// The P2PKH scriptPubKey (`76a914{20}88ac`).
    pub fn p2pkh_script(&self, net: Network) -> Vec<u8> {
        let h = hash160(&self.pubkey);
        let mut s = Vec::with_capacity(25);
        s.push(0x76);
        s.push(0xa9);
        s.push(0x14);
        s.extend_from_slice(&h);
        s.push(0x88);
        s.push(0xac);
        let _ = net;
        s
    }

    /// The bare P2PK scriptPubKey (`21{33}ac`) — coinbases pay this form.
    pub fn p2pk_script(&self) -> Vec<u8> {
        let mut s = Vec::with_capacity(35);
        s.push(0x21);
        s.extend_from_slice(&self.pubkey);
        s.push(0xac);
        s
    }

    /// WIF private key (compressed), importable via `importprivkey`.
    pub fn wif(&self, net: Network) -> String {
        let mut payload = Vec::with_capacity(33);
        payload.extend_from_slice(&self.secret);
        payload.push(0x01); // compressed flag
        base58check_encode(net.wif_version(), &payload)
    }

    /// Decode a WIF key → (secret, version byte). The caller maps the
    /// version byte to a network (mainnet and regtest share prefixes).
    pub fn wif_decode(s: &str) -> Result<([u8; 32], u8), String> {
        let (ver, payload) = base58check_decode(s)?;
        if payload.len() != 33 || payload[32] != 0x01 {
            return Err("WIF must be 32 secret bytes + compression flag".to_string());
        }
        let mut secret = [0u8; 32];
        secret.copy_from_slice(&payload[..32]);
        // Reject the all-zero / out-of-range secrets the node would reject.
        SecretKey::from_slice(&secret).map_err(|e| format!("WIF secret: {e}"))?;
        Ok((secret, ver))
    }
}

/// P2SH address for a 20-byte script hash.
pub fn p2sh_address(script_hash_20: &[u8], net: Network) -> String {
    assert_eq!(script_hash_20.len(), 20);
    base58check_encode(net.p2sh_version(), script_hash_20)
}

/// The P2SH scriptPubKey (`a914{20}87`) for a 20-byte script hash.
pub fn p2sh_script(script_hash_20: &[u8]) -> Vec<u8> {
    assert_eq!(script_hash_20.len(), 20);
    let mut s = Vec::with_capacity(23);
    s.push(0xa9);
    s.push(0x14);
    s.extend_from_slice(script_hash_20);
    s.push(0x87);
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base58check_roundtrip() {
        let payload = hash160(b"lucky lode");
        let s = base58check_encode(111, &payload);
        let (ver, back) = base58check_decode(&s).expect("decode");
        assert_eq!(ver, 111);
        assert_eq!(back, payload);
    }

    #[test]
    fn base58check_rejects_bad_checksum() {
        let mut s = base58check_encode(111, &hash160(b"x"));
        s.pop();
        s.push('1');
        assert!(base58check_decode(&s).is_err());
    }

    #[test]
    fn wif_roundtrip_and_rejects_uncompressed() {
        let k = Keypair::from_secret([7u8; 32]).expect("key");
        let w = k.wif(Network::Testnet);
        // Testnet WIF version is 0x80 (same as Bitcoin mainnet) → K/L.
        assert!(w.starts_with(['K', 'L']), "{w}");
        let (sec, ver) = Keypair::wif_decode(&w).expect("decode");
        assert_eq!(sec, [7u8; 32]);
        assert_eq!(ver, 128);
        // No compression flag → rejected like the node rejects it.
        let raw = bs58::decode(&w).into_vec().expect("b58");
        let noflag = base58check_encode(128, &raw[1..33]);
        assert!(Keypair::wif_decode(&noflag).is_err());
    }

    #[test]
    fn rejects_zero_secret() {
        assert!(Keypair::from_secret([0u8; 32]).is_err());
    }

    #[test]
    fn decodes_node_observed_testnet_addresses() {
        // Rust-generated (secret 07..07), confirmed by the live testnet
        // node (`validateaddress` → isvalid, scriptPubKey 76a914…88ac).
        let (ver, payload) =
            base58check_decode("mvSvTtvD9H9fkgi8MGDyLALgRaR2LhnWFM").expect("decode");
        assert_eq!(ver, 111);
        assert_eq!(
            hex::encode(&payload),
            "a3c6b1ee4a49d9f2af3b3802974744fba924164a"
        );
        // Node-generated (`getnewaddress` on the testnet node).
        let (ver, payload) =
            base58check_decode("mmEmEe9iSoLkfXXp61oBn8LcyyzduwBuz1").expect("decode");
        assert_eq!((ver, payload.len()), (111, 20));
        // Node coinbase recipient (live chain, block 1).
        let (ver, payload) =
            base58check_decode("n2dQr43hTDtDx5XwKnwozN7un4ZbpFzESc").expect("decode");
        assert_eq!((ver, payload.len()), (111, 20));
    }

    #[test]
    fn p2pkh_script_shape() {
        let k = Keypair::from_secret([9u8; 32]).expect("key");
        let s = k.p2pkh_script(Network::Testnet);
        assert_eq!(s.len(), 25);
        assert_eq!(
            (s[0], s[1], s[2], s[23], s[24]),
            (0x76, 0xa9, 0x14, 0x88, 0xac)
        );
        assert_eq!(&s[3..23], &hash160(k.pubkey()));
    }
}
