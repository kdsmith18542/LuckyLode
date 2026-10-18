//! BIP32 hierarchical deterministic wallets (manual implementation over
//! `secp256k1` + `hmac`, pinned to the official BIP32 test vectors).
//!
//! Wallet layout is BIP44: `m / 44' / coin' / account' / change / index`
//! with coin type 1 on testnet/regtest (testnet convention) and 121 on
//! mainnet (Verus-lineage placeholder until the ceremony registers one).
//! Extended keys serialize with the node's version bytes, so `xprv`/`tprv`
//! strings interchange with the node.

use hmac::{Hmac, Mac};
use secp256k1::{PublicKey, Scalar, Secp256k1, SecretKey};
use sha2::Sha512;

use crate::address::{base58_with_checksum, hash160, Keypair};
use crate::chain::Network;

pub const HARDENED: u32 = 0x8000_0000;

fn hmac_sha512(key: &[u8], data: &[u8]) -> [u8; 64] {
    let mut mac = Hmac::<Sha512>::new_from_slice(key).expect("hmac key");
    mac.update(data);
    mac.finalize().into_bytes().into()
}

/// An extended private key (private derivation throughout — the wallet
/// never exports account xpubs in v1).
#[derive(Debug, Clone)]
pub struct XKey {
    net: Network,
    depth: u8,
    parent_fp: [u8; 4],
    child_no: u32,
    chain: [u8; 32],
    secret: [u8; 32],
}

impl XKey {
    /// Master key from a 64-byte BIP39 seed (`HMAC-SHA512("Bitcoin seed")`).
    pub fn master(seed: &[u8], net: Network) -> Result<Self, String> {
        if seed.len() < 16 || seed.len() > 64 {
            return Err("seed must be 16..=64 bytes".to_string());
        }
        let i = hmac_sha512(b"Bitcoin seed", seed);
        let mut secret = [0u8; 32];
        let mut chain = [0u8; 32];
        secret.copy_from_slice(&i[..32]);
        chain.copy_from_slice(&i[32..]);
        SecretKey::from_slice(&secret).map_err(|e| format!("master key: {e}"))?;
        Ok(Self {
            net,
            depth: 0,
            parent_fp: [0u8; 4],
            child_no: 0,
            chain,
            secret,
        })
    }

    fn pubkey_bytes(secret: &[u8; 32]) -> Result<[u8; 33], String> {
        let secp = Secp256k1::new();
        let sk = SecretKey::from_slice(secret).map_err(|e| format!("secret: {e}"))?;
        Ok(PublicKey::from_secret_key(&secp, &sk).serialize())
    }

    fn fingerprint(secret: &[u8; 32]) -> Result<[u8; 4], String> {
        let h = hash160(&Self::pubkey_bytes(secret)?);
        Ok([h[0], h[1], h[2], h[3]])
    }

    /// CKDpriv: derive child `index` (use `HARDENED | i` for hardened).
    pub fn derive(&self, index: u32) -> Result<Self, String> {
        let mut data = Vec::with_capacity(37);
        if index >= HARDENED {
            data.push(0x00);
            data.extend_from_slice(&self.secret);
        } else {
            data.extend_from_slice(&Self::pubkey_bytes(&self.secret)?);
        }
        data.extend_from_slice(&index.to_be_bytes());
        let i = hmac_sha512(&self.chain, &data);
        let mut chain = [0u8; 32];
        chain.copy_from_slice(&i[32..]);
        let tweak = Scalar::from_be_bytes(i[..32].try_into().expect("32 bytes"))
            .map_err(|e| format!("tweak: {e}"))?;
        let sk = SecretKey::from_slice(&self.secret).map_err(|e| format!("parent: {e}"))?;
        let child = sk.add_tweak(&tweak).map_err(|e| format!("derive: {e}"))?;
        let mut secret = [0u8; 32];
        secret.copy_from_slice(&child.secret_bytes());
        Ok(Self {
            net: self.net,
            depth: self.depth + 1,
            parent_fp: Self::fingerprint(&self.secret)?,
            child_no: index,
            chain,
            secret,
        })
    }

    /// Derive a full relative path (e.g. `[44|H, coin|H, acct|H, ch, idx]`).
    pub fn derive_path(&self, path: &[u32]) -> Result<Self, String> {
        let mut k = self.clone();
        for &i in path {
            k = k.derive(i)?;
        }
        Ok(k)
    }

    /// This key as a signing [`Keypair`].
    pub fn keypair(&self) -> Result<Keypair, String> {
        Keypair::from_secret(self.secret)
    }

    /// `xprv…` / `tprv…` (78-byte payload, node-compatible).
    pub fn xprv(&self) -> String {
        let mut b = Vec::with_capacity(78);
        b.extend_from_slice(&self.net.xprv_version());
        b.push(self.depth);
        b.extend_from_slice(&self.parent_fp);
        b.extend_from_slice(&self.child_no.to_be_bytes());
        b.extend_from_slice(&self.chain);
        b.push(0x00);
        b.extend_from_slice(&self.secret);
        base58_with_checksum(&b)
    }

    /// `xpub…` / `tpub…` (neutered: receive-only).
    pub fn xpub(&self) -> Result<String, String> {
        let mut b = Vec::with_capacity(78);
        b.extend_from_slice(&self.net.xpub_version());
        b.push(self.depth);
        b.extend_from_slice(&self.parent_fp);
        b.extend_from_slice(&self.child_no.to_be_bytes());
        b.extend_from_slice(&self.chain);
        b.extend_from_slice(&Self::pubkey_bytes(&self.secret)?);
        Ok(base58_with_checksum(&b))
    }
}

/// First receiving key of an account: `m/44'/coin'/account'/0/0`.
pub fn receive_key(seed: &[u8], net: Network, account: u32, index: u32) -> Result<Keypair, String> {
    external_key(seed, net, account, 0, index)
}

/// Key at `m/44'/coin'/account'/change/index` (change `0` = receive,
/// `1` = change).
pub fn external_key(
    seed: &[u8],
    net: Network,
    account: u32,
    change: u32,
    index: u32,
) -> Result<Keypair, String> {
    XKey::master(seed, net)?
        .derive_path(&[
            HARDENED | 44,
            HARDENED | net.coin_type(),
            HARDENED | account,
            change,
            index,
        ])?
        .keypair()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex_seed(s: &str) -> Vec<u8> {
        hex::decode(s).expect("hex")
    }

    /// BIP32 test vector 1 (mainnet xprv/xpub strings).
    #[test]
    fn vector1() {
        let seed = hex_seed("000102030405060708090a0b0c0d0e0f");
        let m = XKey::master(&seed, Network::Mainnet).expect("master");
        assert_eq!(m.xprv(), "xprv9s21ZrQH143K3QTDL4LXw2F7HEK3wJUD2nW2nRk4stbPy6cq3jPPqjiChkVvvNKmPGJxWUtg6LnF5kejMRNNU3TGtRBeJgk33yuGBxrMPHi");
        assert_eq!(m.xpub().expect("xpub"), "xpub661MyMwAqRbcFtXgS5sYJABqqG9YLmC4Q1Rdap9gSE8NqtwybGhePY2gZ29ESFjqJoCu1Rupje8YtGqsefD265TMg7usUDFdp6W1EGMcet8");
        let c = m.derive_path(&[HARDENED]).expect("m/0H");
        assert_eq!(c.xprv(), "xprv9uHRZZhk6KAJC1avXpDAp4MDc3sQKNxDiPvvkX8Br5ngLNv1TxvUxt4cV1rGL5hj6KCesnDYUhd7oWgT11eZG7XnxHrnYeSvkzY7d2bhkJ7");
        assert_eq!(c.xpub().expect("xpub"), "xpub68Gmy5EdvgibQVfPdqkBBCHxA5htiqg55crXYuXoQRKfDBFA1WEjWgP6LHhwBZeNK1VTsfTFUHCdrfp1bgwQ9xv5ski8PX9rL2dZXvgGDnw");
        let c = c.derive(1).expect("m/0H/1");
        assert_eq!(c.xprv(), "xprv9wTYmMFdV23N2TdNG573QoEsfRrWKQgWeibmLntzniatZvR9BmLnvSxqu53Kw1UmYPxLgboyZQaXwTCg8MSY3H2EU4pWcQDnRnrVA1xe8fs");
        let c = c.derive(HARDENED | 2).expect("m/0H/1/2H");
        assert_eq!(c.xprv(), "xprv9z4pot5VBttmtdRTWfWQmoH1taj2axGVzFqSb8C9xaxKymcFzXBDptWmT7FwuEzG3ryjH4ktypQSAewRiNMjANTtpgP4mLTj34bhnZX7UiM");
        let c = c.derive(2).expect("m/0H/1/2H/2");
        assert_eq!(c.xprv(), "xprvA2JDeKCSNNZky6uBCviVfJSKyQ1mDYahRjijr5idH2WwLsEd4Hsb2Tyh8RfQMuPh7f7RtyzTtdrbdqqsunu5Mm3wDvUAKRHSC34sJ7in334");
        let c = c.derive(1_000_000_000).expect("m/0H/1/2H/2/1e9");
        assert_eq!(c.xprv(), "xprvA41z7zogVVwxVSgdKUHDy1SKmdb533PjDz7J6N6mV6uS3ze1ai8FHa8kmHScGpWmj4WggLyQjgPie1rFSruoUihUZREPSL39UNdE3BBDu76");
        assert_eq!(c.xpub().expect("xpub"), "xpub6H1LXWLaKsWFhvm6RVpEL9P4KfRZSW7abD2ttkWP3SSQvnyA8FSVqNTEcYFgJS2UaFcxupHiYkro49S8yGasTvXEYBVPamhGW6cFJodrTHy");
    }

    /// BIP32 test vector 2 (long seed, mixed hardened).
    #[test]
    fn vector2() {
        let seed = hex_seed("fffcf9f6f3f0edeae7e4e1dedbd8d5d2cfccc9c6c3c0bdbab7b4b1aeaba8a5a29f9c999693908d8a8784817e7b7875726f6c696663605d5a5754514e4b484542");
        let m = XKey::master(&seed, Network::Mainnet).expect("master");
        assert_eq!(m.xprv(), "xprv9s21ZrQH143K31xYSDQpPDxsXRTUcvj2iNHm5NUtrGiGG5e2DtALGdso3pGz6ssrdK4PFmM8NSpSBHNqPqm55Qn3LqFtT2emdEXVYsCzC2U");
        let c = m.derive(0).expect("m/0");
        assert_eq!(c.xprv(), "xprv9vHkqa6EV4sPZHYqZznhT2NPtPCjKuDKGY38FBWLvgaDx45zo9WQRUT3dKYnjwih2yJD9mkrocEZXo1ex8G81dwSM1fwqWpWkeS3v86pgKt");
        let c = c.derive(HARDENED | 2147483647).expect("m/0/2147483647H");
        assert_eq!(c.xprv(), "xprv9wSp6B7kry3Vj9m1zSnLvN3xH8RdsPP1Mh7fAaR7aRLcQMKTR2vidYEeEg2mUCTAwCd6vnxVrcjfy2kRgVsFawNzmjuHc2YmYRmagcEPdU9");
        let c = c.derive(1).expect("m/0/2147483647H/1");
        assert_eq!(c.xprv(), "xprv9zFnWC6h2cLgpmSA46vutJzBcfJ8yaJGg8cX1e5StJh45BBciYTRXSd25UEPVuesF9yog62tGAQtHjXajPPdbRCHuWS6T8XA2ECKADdw4Ef");
        let c = c.derive(HARDENED | 2147483646).expect("m/0/2147483647H/1/2147483646H");
        assert_eq!(c.xpub().expect("xpub"), "xpub6ERApfZwUNrhLCkDtcHTcxd75RbzS1ed54G1LkBUHQVHQKqhMkhgbmJbZRkrgZw4koxb5JaHWkY4ALHY2grBGRjaDMzQLcgJvLJuZZvRcEL");
        let c = c.derive(2).expect("m/0/2147483647H/1/2147483646H/2");
        assert_eq!(c.xpub().expect("xpub"), "xpub6FnCn6nSzZAw5Tw7cgR9bi15UV96gLZhjDstkXXxvCLsUXBGXPdSnLFbdpq8p9HmGsApME5hQTZ3emM2rnY5agb9rXpVGyy3bdW6EEgAtqt");
    }

    /// BIP32 vectors 3+4: leading-zero retention in derivation.
    #[test]
    fn vectors3_and_4_leading_zeros() {
        let seed = hex_seed("4b381541583be4423346c643850da4b320e46a87ae3d2a4e6da11eba819cd4acba45d239319ac14f863b8d5ab5a0d0c64d2e8a1e7d1457df2e5a3c51c73235be");
        let m = XKey::master(&seed, Network::Mainnet).expect("master");
        assert_eq!(m.xprv(), "xprv9s21ZrQH143K25QhxbucbDDuQ4naNntJRi4KUfWT7xo4EKsHt2QJDu7KXp1A3u7Bi1j8ph3EGsZ9Xvz9dGuVrtHHs7pXeTzjuxBrCmmhgC6");
        let c = m.derive(HARDENED).expect("m/0H");
        assert_eq!(c.xprv(), "xprv9uPDJpEQgRQfDcW7BkF7eTya6RPxXeJCqCJGHuCJ4GiRVLzkTXBAJMu2qaMWPrS7AANYqdq6vcBcBUdJCVVFceUvJFjaPdGZ2y9WACViL4L");

        let seed = hex_seed("3ddd5602285899a946114506157c7997e5444528f3003f6134712147db19b678");
        let m = XKey::master(&seed, Network::Mainnet).expect("master");
        assert_eq!(m.xprv(), "xprv9s21ZrQH143K48vGoLGRPxgo2JNkJ3J3fqkirQC2zVdk5Dgd5w14S7fRDyHH4dWNHUgkvsvNDCkvAwcSHNAQwhwgNMgZhLtQC63zxwhQmRv");
        let c = m.derive(HARDENED).expect("m/0H");
        assert_eq!(c.xprv(), "xprv9vB7xEWwNp9kh1wQRfCCQMnZUEG21LpbR9NPCNN1dwhiZkjjeGRnaALmPXCX7SgjFTiCTT6bXes17boXtjq3xLpcDjzEuGLQBM5ohqkao9G");
        let c = c.derive(HARDENED | 1).expect("m/0H/1H");
        assert_eq!(c.xprv(), "xprv9xJocDuwtYCMNAo3Zw76WENQeAS6WGXQ55RCy7tDJ8oALr4FWkuVoHJeHVAcAqiZLE7Je3vZJHxspZdFHfnBEjHqU5hG1Jaj32dVoS6XLT1");
    }

    /// Testnet xkeys use tprv/tpub magic (node-compatible).
    #[test]
    fn testnet_xkey_prefixes() {
        let seed = hex_seed("000102030405060708090a0b0c0d0e0f");
        let m = XKey::master(&seed, Network::Testnet).expect("master");
        assert!(m.xprv().starts_with("tprv"), "{}", m.xprv());
        assert!(m.xpub().expect("xpub").starts_with("tpub"));
    }
}
