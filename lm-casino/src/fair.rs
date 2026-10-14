//! Provably-fair commit-reveal RNG (doc 14 §14.2).
//!
//! Model (Stake-style, HMAC-SHA256):
//!
//! 1. The server picks a 32-byte secret `server_seed` and publishes
//!    `commitment = SHA256(server_seed)` before play.
//! 2. The player supplies a `client_seed`.
//! 3. The `cursor`-th random value of the round is the first 8 bytes
//!    (little-endian) of
//!    `HMAC-SHA256(key = server_seed, msg = client_seed ‖ nonce_le64 ‖ cursor_le64)`.
//! 4. The server seed is revealed when rotated, so any player can recompute
//!    every past spin with [`verify_spin`] and confirm it against the
//!    published commitment with [`verify_commitment`].
//!
//! The HKDF/HMAC construction is implemented directly over `sha2` to keep the
//! dependency surface minimal; it is the textbook RFC 2104 HMAC.

use sha2::{Digest, Sha256};

/// SHA-256 digest length.
pub const HASH_LEN: usize = 32;
/// HMAC block size for SHA-256.
const BLOCK_LEN: usize = 64;

/// SHA-256 of `data`.
#[must_use]
pub fn sha256(data: &[u8]) -> [u8; HASH_LEN] {
    let mut h = Sha256::new();
    h.update(data);
    h.finalize().into()
}

/// RFC 2104 HMAC-SHA256.
#[must_use]
pub fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; HASH_LEN] {
    let mut k = [0u8; BLOCK_LEN];
    if key.len() > BLOCK_LEN {
        k[..HASH_LEN].copy_from_slice(&sha256(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; BLOCK_LEN];
    let mut opad = [0x5cu8; BLOCK_LEN];
    for i in 0..BLOCK_LEN {
        ipad[i] ^= k[i];
        opad[i] ^= k[i];
    }
    let mut inner = Sha256::new();
    inner.update(ipad);
    inner.update(msg);
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner);
    outer.finalize().into()
}

/// Publishable commitment to a server seed.
#[must_use]
pub fn commitment(server_seed: &[u8]) -> [u8; HASH_LEN] {
    sha256(server_seed)
}

/// True iff `committed` is the commitment to `server_seed`.
#[must_use]
pub fn verify_commitment(server_seed: &[u8], committed: &[u8; HASH_LEN]) -> bool {
    &commitment(server_seed) == committed
}

/// The `cursor`-th 64-bit draw of round `(server_seed, client_seed, nonce)`.
#[must_use]
pub fn draw_u64(server_seed: &[u8], client_seed: &[u8], nonce: u64, cursor: u64) -> u64 {
    let mut msg = Vec::with_capacity(client_seed.len() + 16);
    msg.extend_from_slice(client_seed);
    msg.extend_from_slice(&nonce.to_le_bytes());
    msg.extend_from_slice(&cursor.to_le_bytes());
    let mac = hmac_sha256(server_seed, &msg);
    u64::from_le_bytes(mac[..8].try_into().expect("8 bytes"))
}

/// A draw as a uniform float in `[0, 1)`.
#[must_use]
pub fn draw_float(server_seed: &[u8], client_seed: &[u8], nonce: u64, cursor: u64) -> f64 {
    draw_u64(server_seed, client_seed, nonce, cursor) as f64 / (u64::MAX as f64 + 1.0)
}

/// Uniform integer in `[0, modulo)` (modulo bias ≤ 2⁻⁶⁴ · modulo, negligible
/// for symbol counts).
///
/// # Panics
/// Panics if `modulo == 0`.
#[must_use]
pub fn draw_index(
    server_seed: &[u8],
    client_seed: &[u8],
    nonce: u64,
    cursor: u64,
    modulo: u64,
) -> u64 {
    assert!(modulo > 0, "modulo must be positive");
    draw_u64(server_seed, client_seed, nonce, cursor) % modulo
}

/// Recompute a spin's reel stops from a revealed server seed — the player-side
/// verifier. `symbols_per_reel[i]` is the number of stops on reel `i`; stops are
/// drawn from consecutive cursors starting at `start_cursor`.
#[must_use]
pub fn verify_spin(
    server_seed: &[u8],
    client_seed: &[u8],
    nonce: u64,
    start_cursor: u64,
    symbols_per_reel: &[u64],
) -> Vec<u64> {
    symbols_per_reel
        .iter()
        .enumerate()
        .map(|(i, count)| {
            draw_index(
                server_seed,
                client_seed,
                nonce,
                start_cursor + i as u64,
                *count,
            )
        })
        .collect()
}

/// Lowercase hex encoding.
#[must_use]
pub fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Decode lowercase/uppercase hex.
///
/// # Errors
/// `Err` on odd length or non-hex characters.
pub fn from_hex(s: &str) -> Result<Vec<u8>, String> {
    if !s.len().is_multiple_of(2) {
        return Err("odd hex length".to_string());
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(s.len() / 2);
    for pair in bytes.chunks(2) {
        let hi = (pair[0] as char)
            .to_digit(16)
            .ok_or_else(|| format!("bad hex: {s}"))?;
        let lo = (pair[1] as char)
            .to_digit(16)
            .ok_or_else(|| format!("bad hex: {s}"))?;
        out.push(((hi << 4) | lo) as u8);
    }
    Ok(out)
}

/// Mutable fair-play session: holds the secret server seed, the player's client
/// seed, the round nonce and a cursor. Callers advance the cursor by drawing.
#[derive(Debug, Clone)]
pub struct Fair {
    server_seed: [u8; HASH_LEN],
    client_seed: Vec<u8>,
    nonce: u64,
    cursor: u64,
}

impl Fair {
    /// Start a session from a server seed, client seed and round nonce.
    pub fn new(server_seed: [u8; HASH_LEN], client_seed: impl Into<Vec<u8>>, nonce: u64) -> Self {
        Self {
            server_seed,
            client_seed: client_seed.into(),
            nonce,
            cursor: 0,
        }
    }

    /// The commitment to publish before play.
    #[must_use]
    pub fn commitment(&self) -> [u8; HASH_LEN] {
        commitment(&self.server_seed)
    }

    /// The cursor at which the next draw will occur.
    #[must_use]
    pub fn cursor(&self) -> u64 {
        self.cursor
    }

    /// The round nonce.
    #[must_use]
    pub fn nonce(&self) -> u64 {
        self.nonce
    }

    /// Next 64-bit draw, advancing the cursor.
    pub fn next_u64(&mut self) -> u64 {
        let v = draw_u64(
            &self.server_seed,
            &self.client_seed,
            self.nonce,
            self.cursor,
        );
        self.cursor += 1;
        v
    }

    /// Next uniform index in `[0, modulo)`.
    ///
    /// # Panics
    /// Panics if `modulo == 0`.
    pub fn next_index(&mut self, modulo: u64) -> u64 {
        assert!(modulo > 0, "modulo must be positive");
        self.next_u64() % modulo
    }

    /// Draw a full spin's reel stops (one cursor per reel).
    pub fn next_reel_stops(&mut self, symbols_per_reel: &[u64]) -> Vec<u64> {
        symbols_per_reel
            .iter()
            .map(|c| self.next_index(*c))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trips() {
        assert_eq!(to_hex(&[0x00, 0xff, 0x1a]), "00ff1a");
        assert_eq!(from_hex("00ff1a").unwrap(), vec![0x00, 0xff, 0x1a]);
        assert!(from_hex("0").is_err());
        assert!(from_hex("zz").is_err());
    }

    #[test]
    fn commitment_is_sha256_of_seed() {
        let seed = [0xABu8; 32];
        let c = commitment(&seed);
        assert!(verify_commitment(&seed, &c));
        assert!(!verify_commitment(&[0xACu8; 32], &c));
    }

    #[test]
    fn kat_hmac_commitment_and_draw() {
        // Known-answer vectors (independently reproduced with a reference
        // HMAC-SHA256/SHA-256 implementation).
        let seed = [0x00u8; 32];
        let mac = hmac_sha256(&seed, b"abc");
        assert_eq!(
            to_hex(&mac),
            "fd7adb152c05ef80dccf50a1fa4c05d5a3ec6da95575fc312ae7c5d091836351"
        );
        assert_eq!(
            to_hex(&commitment(&seed)),
            "66687aadf862bd776c8fc18b8e9f8e20089714856ee233b3902a591d0d5f2925"
        );
        // draw_u64 = first 8 LE bytes of HMAC(seed, b"client" ‖ nonce=1 ‖ cursor=0).
        assert_eq!(draw_u64(&seed, b"client", 1, 0), 11_640_879_306_816_409_602);
    }

    #[test]
    fn deterministic_and_input_sensitive() {
        let seed = [7u8; 32];
        let a = draw_u64(&seed, b"client", 1, 0);
        let b = draw_u64(&seed, b"client", 1, 0);
        assert_eq!(a, b);
        assert_ne!(a, draw_u64(&seed, b"client", 2, 0));
        assert_ne!(a, draw_u64(&seed, b"other", 1, 0));
        assert_ne!(a, draw_u64(&seed, b"client", 1, 1));
    }

    #[test]
    fn verify_spin_reproduces_session() {
        let seed = [42u8; 32];
        let counts = [16u64, 16, 16, 16, 16];
        let mut fair = Fair::new(seed, "player-seed", 9);
        let live = fair.next_reel_stops(&counts);
        let replayed = verify_spin(&seed, b"player-seed", 9, 0, &counts);
        assert_eq!(live, replayed);
        for (s, c) in live.iter().zip(counts) {
            assert!(*s < c);
        }
    }

    #[test]
    fn draws_are_close_to_uniform() {
        let seed = [3u8; 32];
        let n = 200_000u64;
        let mut buckets = [0u32; 10];
        let mut sum = 0f64;
        for i in 0..n {
            let f = draw_float(&seed, b"c", 0, i);
            assert!((0.0..1.0).contains(&f));
            sum += f;
            buckets[(f * 10.0) as usize] += 1;
        }
        let mean = sum / n as f64;
        assert!((mean - 0.5).abs() < 0.01, "mean {mean}");
        let expect = n / 10;
        for (i, b) in buckets.iter().enumerate() {
            let dev = (*b as f64 - expect as f64).abs() / expect as f64;
            assert!(dev < 0.1, "bucket {i} dev {dev}");
        }
    }

    #[test]
    fn cursor_advances_per_reel() {
        let mut fair = Fair::new([1u8; 32], "c", 0);
        assert_eq!(fair.cursor(), 0);
        fair.next_reel_stops(&[10, 10, 10]);
        assert_eq!(fair.cursor(), 3);
        fair.next_u64();
        assert_eq!(fair.cursor(), 4);
    }
}
