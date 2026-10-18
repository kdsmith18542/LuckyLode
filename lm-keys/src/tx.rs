//! Transparent transaction model: legacy (sprout-era, v1) serialization
//! plus the overwintered v3/v4 envelope for the post-upgrade future.
//!
//! Byte layout mirrors `CTransaction::SerializationOp`
//! (verus/src/primitives/transaction.h): a 4-byte header that is the plain
//! version when not overwintered, else `version | (1 << 31)` followed by
//! the version-group id; expiry/joinsplit sections only in the matching
//! formats. Deserialization is deliberately tolerant of a missing
//! joinsplit-count byte on legacy v2 (lets Bitcoin-format vectors decode).

use crate::address::sha256d;

pub const SIGHASH_ALL: u32 = 1;
pub const SEQUENCE_FINAL: u32 = 0xFFFF_FFFF;

/// Overwinter/sapling version groups (standard Zcash ids, same in the fork).
pub const OVERWINTER_GROUP_ID: u32 = 0x03C4_8270;
pub const SAPLING_GROUP_ID: u32 = 0x892F_2085;
pub const OVERWINTER_TX_VERSION: i32 = 3;
pub const SAPLING_TX_VERSION: i32 = 4;

/// Outpoint: txid in wire (natural) byte order + output index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutPoint {
    pub txid: [u8; 32],
    pub vout: u32,
}

#[derive(Debug, Clone)]
pub struct TxIn {
    pub prevout: OutPoint,
    pub script_sig: Vec<u8>,
    pub sequence: u32,
}

#[derive(Debug, Clone)]
pub struct TxOut {
    pub value: i64,
    pub script_pubkey: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct Transaction {
    pub overwintered: bool,
    pub version: i32,
    pub version_group_id: Option<u32>,
    pub vin: Vec<TxIn>,
    pub vout: Vec<TxOut>,
    pub locktime: u32,
    pub expiry_height: Option<u32>,
}

impl Transaction {
    /// Legacy v1 transaction (what testnet/regtest carry today).
    pub fn legacy(vin: Vec<TxIn>, vout: Vec<TxOut>) -> Self {
        Self {
            overwintered: false,
            version: 1,
            version_group_id: None,
            vin,
            vout,
            locktime: 0,
            expiry_height: None,
        }
    }

    /// Overwintered v3 transaction (ZIP-143 epoch). `expiry_height = 0`
    /// means "never expires" (IsExpiredTx, main.cpp:926).
    pub fn overwinter_v3(vin: Vec<TxIn>, vout: Vec<TxOut>) -> Self {
        Self {
            overwintered: true,
            version: OVERWINTER_TX_VERSION,
            version_group_id: Some(OVERWINTER_GROUP_ID),
            vin,
            vout,
            locktime: 0,
            expiry_height: Some(0),
        }
    }

    /// Overwintered v4 transaction (ZIP-243 epoch, transparent-only:
    /// empty shielded/joinsplit sections are serialized as zero counts).
    pub fn sapling_v4(vin: Vec<TxIn>, vout: Vec<TxOut>) -> Self {
        Self {
            overwintered: true,
            version: SAPLING_TX_VERSION,
            version_group_id: Some(SAPLING_GROUP_ID),
            vin,
            vout,
            locktime: 0,
            expiry_height: Some(0),
        }
    }

    fn is_sapling_v4(&self) -> bool {
        self.overwintered
            && self.version == SAPLING_TX_VERSION
            && self.version_group_id == Some(SAPLING_GROUP_ID)
    }

    pub fn serialize(&self) -> Vec<u8> {
        let mut b = Vec::new();
        if self.overwintered {
            put_u32(&mut b, (self.version as u32) | (1 << 31));
            put_u32(&mut b, self.version_group_id.unwrap_or(0));
        } else {
            put_u32(&mut b, self.version as u32);
        }
        put_compactsize(&mut b, self.vin.len() as u64);
        for i in &self.vin {
            b.extend_from_slice(&i.prevout.txid);
            put_u32(&mut b, i.prevout.vout);
            put_bytes(&mut b, &i.script_sig);
            put_u32(&mut b, i.sequence);
        }
        put_compactsize(&mut b, self.vout.len() as u64);
        for o in &self.vout {
            put_i64(&mut b, o.value);
            put_bytes(&mut b, &o.script_pubkey);
        }
        put_u32(&mut b, self.locktime);
        if self.overwintered {
            put_u32(&mut b, self.expiry_height.unwrap_or(0));
            if self.is_sapling_v4() {
                put_i64(&mut b, 0); // valueBalance (transparent-only)
                put_compactsize(&mut b, 0); // shielded spends
                put_compactsize(&mut b, 0); // shielded outputs
            }
            // Joinsplit section (nVersion >= 2): empty.
            put_compactsize(&mut b, 0);
        } else if self.version >= 2 {
            // Fork consensus: v2+ carries the joinsplit section (empty here).
            put_compactsize(&mut b, 0);
        }
        b
    }

    pub fn deserialize(raw: &[u8]) -> Result<(Self, usize), String> {
        let mut r = Reader { b: raw, off: 0 };
        let header = r.u32()?;
        let overwintered = header >> 31 != 0;
        let version = (header & 0x7FFF_FFFF) as i32;
        let version_group_id = if overwintered { Some(r.u32()?) } else { None };
        let n_in = r.compactsize()? as usize;
        if n_in > 100_000 {
            return Err("absurd input count".to_string());
        }
        let mut vin = Vec::with_capacity(n_in.min(1024));
        for _ in 0..n_in {
            let mut txid = [0u8; 32];
            r.bytes(&mut txid)?;
            vin.push(TxIn {
                prevout: OutPoint {
                    txid,
                    vout: r.u32()?,
                },
                script_sig: r.vec()?,
                sequence: r.u32()?,
            });
        }
        let n_out = r.compactsize()? as usize;
        if n_out > 100_000 {
            return Err("absurd output count".to_string());
        }
        let mut vout = Vec::with_capacity(n_out.min(1024));
        for _ in 0..n_out {
            vout.push(TxOut {
                value: r.i64()?,
                script_pubkey: r.vec()?,
            });
        }
        let locktime = r.u32()?;
        let (expiry_height, sapling_v4) = if overwintered {
            let e = Some(r.u32()?);
            let s4 = version == SAPLING_TX_VERSION && version_group_id == Some(SAPLING_GROUP_ID);
            (e, s4)
        } else {
            (None, false)
        };
        if sapling_v4 {
            // Transparent-only: refuse shielded sections outright.
            let vb = r.i64()?;
            if vb != 0 {
                return Err("shielded valueBalance unsupported".to_string());
            }
            if r.compactsize()? != 0 || r.compactsize()? != 0 {
                return Err("shielded sections unsupported".to_string());
            }
        }
        if overwintered {
            // Consensus always carries the (empty) joinsplit count here.
            if r.compactsize()? != 0 {
                return Err("joinsplits unsupported".to_string());
            }
        } else if version >= 2 && !r.eof() {
            // Fork format carries an (empty) joinsplit count on v2+; plain
            // Bitcoin v2 vectors end here — accept both.
            if r.compactsize()? != 0 {
                return Err("joinsplits unsupported".to_string());
            }
        }
        let used = r.off;
        Ok((
            Self {
                overwintered,
                version,
                version_group_id,
                vin,
                vout,
                locktime,
                expiry_height,
            },
            used,
        ))
    }

    /// Natural-order txid (display = reversed hex, like explorers).
    pub fn txid(&self) -> [u8; 32] {
        sha256d(&self.serialize())
    }
}

/// Explorer-style txid hex (reversed byte order).
pub fn txid_display(txid: &[u8; 32]) -> String {
    let mut r = *txid;
    r.reverse();
    hex::encode(r)
}

/// Parse a display-order txid hex into wire-order bytes.
pub fn parse_txid_display(s: &str) -> Result<[u8; 32], String> {
    let v = hex::decode(s).map_err(|e| format!("txid hex: {e}"))?;
    if v.len() != 32 {
        return Err("txid must be 32 bytes".to_string());
    }
    let mut b = [0u8; 32];
    b.copy_from_slice(&v);
    b.reverse();
    Ok(b)
}

pub(crate) fn put_u32(b: &mut Vec<u8>, v: u32) {    b.extend_from_slice(&v.to_le_bytes());
}

pub(crate) fn put_i64(b: &mut Vec<u8>, v: i64) {
    b.extend_from_slice(&v.to_le_bytes());
}

pub(crate) fn put_compactsize(b: &mut Vec<u8>, v: u64) {
    if v < 0xfd {
        b.push(v as u8);
    } else if v <= 0xffff {
        b.push(0xfd);
        b.extend_from_slice(&(v as u16).to_le_bytes());
    } else if v <= 0xffff_ffff {
        b.push(0xfe);
        b.extend_from_slice(&(v as u32).to_le_bytes());
    } else {
        b.push(0xff);
        b.extend_from_slice(&v.to_le_bytes());
    }
}

pub(crate) fn put_bytes(b: &mut Vec<u8>, v: &[u8]) {
    put_compactsize(b, v.len() as u64);
    b.extend_from_slice(v);
}

struct Reader<'a> {
    b: &'a [u8],
    off: usize,
}

impl<'a> Reader<'a> {
    fn eof(&self) -> bool {
        self.off >= self.b.len()
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        if self.off + n > self.b.len() {
            return Err("truncated transaction".to_string());
        }
        let s = &self.b[self.off..self.off + n];
        self.off += n;
        Ok(s)
    }

    fn bytes(&mut self, out: &mut [u8]) -> Result<(), String> {
        let n = out.len();
        out.copy_from_slice(self.take(n)?);
        Ok(())
    }

    fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().expect("4 bytes")))
    }

    fn i64(&mut self) -> Result<i64, String> {
        Ok(i64::from_le_bytes(self.take(8)?.try_into().expect("8 bytes")))
    }

    fn compactsize(&mut self) -> Result<u64, String> {
        let f = self.take(1)?[0];
        match f {
            0xfd => Ok(u16::from_le_bytes(self.take(2)?.try_into().expect("2 bytes")) as u64),
            0xfe => Ok(u32::from_le_bytes(self.take(4)?.try_into().expect("4 bytes")) as u64),
            0xff => Ok(u64::from_le_bytes(self.take(8)?.try_into().expect("8 bytes"))),
            n => Ok(n as u64),
        }
    }

    fn vec(&mut self) -> Result<Vec<u8>, String> {
        let n = self.compactsize()? as usize;
        if n > 10_000_000 {
            return Err("absurd item length".to_string());
        }
        Ok(self.take(n)?.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compactsize_boundaries() {
        for v in [0u64, 1, 252, 253, 0xffff, 0x10000, 0xffff_ffff] {
            let mut b = Vec::new();
            put_compactsize(&mut b, v);
            let mut r = Reader { b: &b, off: 0 };
            assert_eq!(r.compactsize().expect("read"), v);
        }
    }

    #[test]
    fn txid_display_roundtrip() {
        let raw = hex::decode("00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff")
            .expect("hex");
        let mut wire = [0u8; 32];
        wire.copy_from_slice(&raw);
        let disp = txid_display(&wire);
        assert_eq!(
            disp,
            "ffeeddccbbaa99887766554433221100ffeeddccbbaa99887766554433221100"
        );
        assert_eq!(parse_txid_display(&disp).expect("parse"), wire);
    }
}
