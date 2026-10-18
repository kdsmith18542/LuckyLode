//! Stratum V2 secure session (sv2-spec 04-Protocol-Security): the Noise NX
//! handshake over secp256k1 with ElligatorSwift-encoded handshake keys, then
//! a ChaCha20-Poly1305 encrypted transport carrying [`crate::v2`] frames.
//!
//! Crypto is delegated to the reference `noise_sv2` crate (the same crate the
//! reference pool and miner use), so wire compatibility is by construction:
//!
//! - **Handshake (NX, two messages):** the initiator sends a 64-byte
//!   ElligatorSwift ephemeral; the responder answers with 234 bytes (its
//!   ephemeral + the encrypted static key + the encrypted certificate).
//! - **Responder authentication:** the certificate is a Schnorr signature by
//!   the pool's authority key. Pinning that key on the miner side
//!   ([`V2Session::connect`] with `Some(pk)`) authenticates the pool; without
//!   it the session is encrypted but unauthenticated (documented, never
//!   silent).
//! - **Transport (spec §4.6):** the 6-byte frame header is encrypted on its
//!   own (22 bytes), then the payload is split into 65519-byte plaintext
//!   blocks, each sealed into a 65535-byte ciphertext. The header's `U24`
//!   length is the *plaintext* payload length; [`ciphertext_len`] converts it.
//!   The AEAD rejects any modified byte.
//!
//! Version negotiation rides on top: [`setup_connection`] /
//! [`accept_setup_connection`] exchange `SetupConnection` (and its
//! success/error replies) inside the encrypted channel.

use std::fmt;
use std::io::{Read, Write};
use std::time::Duration;

use noise_sv2::{Initiator, NoiseDecryptor, NoiseEncryptor, Responder};
use secp256k1::XOnlyPublicKey;

use crate::v2::{
    from_frame, to_frame, CodecError, Frame, FrameError, SetupConnection, SetupConnectionError,
    SetupConnectionSuccess, MAX_FRAME_LEN, MSG_SETUP_CONNECTION, MSG_SETUP_CONNECTION_ERROR,
    MSG_SETUP_CONNECTION_SUCCESS,
};

/// Initiator handshake message: the ElligatorSwift-encoded ephemeral key.
pub const HANDSHAKE_INITIATOR_LEN: usize = 64;
/// Responder handshake message: ephemeral + encrypted static + certificate.
pub const HANDSHAKE_RESPONDER_LEN: usize = 234;
/// ChaCha20-Poly1305 authentication tag added to every encrypted block.
pub const MAC_LEN: usize = 16;
/// Frame header plaintext length (spec §4.6).
pub const NOISE_HEADER_LEN: usize = 6;
/// The header is encrypted on its own: 6 plaintext + 16 tag.
pub const NOISE_ENCRYPTED_HEADER_LEN: usize = NOISE_HEADER_LEN + MAC_LEN;
/// Largest plaintext block per AEAD ciphertext (65535 - 16).
pub const MAX_PLAINTEXT_CHUNK: usize = 65519;
/// Largest ciphertext block (the Noise protocol message limit).
pub const MAX_CIPHERTEXT_CHUNK: usize = 65535;

/// Ciphertext length for a plaintext payload length (spec §4.6):
/// 65519-byte plaintext blocks become 65535-byte ciphertexts, and a trailing
/// partial block gains one 16-byte tag.
#[must_use]
pub fn ciphertext_len(plaintext_len: usize) -> usize {
    let full = plaintext_len / MAX_PLAINTEXT_CHUNK;
    let rem = plaintext_len % MAX_PLAINTEXT_CHUNK;
    full * MAX_CIPHERTEXT_CHUNK + if rem > 0 { rem + MAC_LEN } else { 0 }
}

/// Secure-session error.
#[derive(Debug)]
pub enum SessionError {
    /// Underlying stream I/O failed.
    Io(std::io::Error),
    /// The handshake failed (bad authority key, tampered handshake message,
    /// or an expired certificate).
    Handshake(String),
    /// An encrypted block failed authentication (tampered or truncated).
    Decrypt,
    /// A decrypted frame was not a valid V2 frame.
    Frame(FrameError),
    /// A decrypted frame carried an unexpected message type.
    UnexpectedMessage(u8),
    /// A message payload failed to decode.
    Codec(CodecError),
    /// The peer rejected the connection (`SetupConnection.Error` code).
    Rejected(String),
    /// The frame payload exceeds the crate's frame cap.
    TooLong(usize),
}

impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SessionError::Io(e) => write!(f, "io: {e}"),
            SessionError::Handshake(m) => write!(f, "handshake: {m}"),
            SessionError::Decrypt => write!(f, "encrypted block failed authentication"),
            SessionError::Frame(e) => write!(f, "frame: {e}"),
            SessionError::UnexpectedMessage(t) => write!(f, "unexpected message type 0x{t:02x}"),
            SessionError::Codec(e) => write!(f, "codec: {e:?}"),
            SessionError::Rejected(c) => write!(f, "peer rejected: {c}"),
            SessionError::TooLong(n) => write!(f, "frame payload {n} exceeds the frame cap"),
        }
    }
}

impl std::error::Error for SessionError {}

impl From<std::io::Error> for SessionError {
    fn from(e: std::io::Error) -> Self {
        SessionError::Io(e)
    }
}

impl From<FrameError> for SessionError {
    fn from(e: FrameError) -> Self {
        SessionError::Frame(e)
    }
}

/// Generate a pool authority keypair: `(x-only public, secret)`, both 32
/// bytes. The miner pins the public key; the pool keeps the secret.
#[must_use]
pub fn generate_authority() -> ([u8; 32], [u8; 32]) {
    let secp = secp256k1::Secp256k1::new();
    loop {
        let secret = random_secret();
        if let Ok(sk) = secp256k1::SecretKey::from_slice(&secret) {
            let kp = secp256k1::Keypair::from_secret_key(&secp, &sk);
            let (xonly, _parity) = kp.x_only_public_key();
            return (xonly.serialize(), kp.secret_bytes());
        }
    }
}

/// Base58 alphabet (Bitcoin order).
const B58: &[u8; 58] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";

/// Encode a pool authority public key for the mining URL (spec §4.7):
/// base58check over the little-endian `[1, 0]` version prefix followed by
/// the 32-byte x-only key. The pool publishes this; miners paste it into
/// `stratum2+tcp://host:port/<key>`.
#[must_use]
pub fn authority_url_key(public: &[u8; 32]) -> String {
    let mut payload = Vec::with_capacity(34);
    payload.extend_from_slice(&[1, 0]);
    payload.extend_from_slice(public);
    base58check_encode(&payload)
}

/// Decode and validate an authority key from a URL path (spec §4.7):
/// base58check, `[1, 0]` version prefix, 32-byte key, correct checksum.
///
/// # Errors
/// A reason string when the encoding, length, prefix or checksum is bad.
pub fn parse_authority_url_key(s: &str) -> Result<[u8; 32], String> {
    let data = base58_decode(s)?;
    if data.len() != 38 {
        return Err(format!("expected 38 bytes, got {}", data.len()));
    }
    let (payload, checksum) = data.split_at(34);
    let expected = sha256d(payload);
    if checksum != &expected[..4] {
        return Err("bad checksum".to_string());
    }
    if payload[0] != 1 || payload[1] != 0 {
        return Err("unsupported version prefix (expected [1, 0])".to_string());
    }
    let mut key = [0u8; 32];
    key.copy_from_slice(&payload[2..]);
    Ok(key)
}

fn sha256d(data: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let first = Sha256::digest(data);
    Sha256::digest(first).into()
}

fn base58check_encode(payload: &[u8]) -> String {
    let mut data = payload.to_vec();
    let checksum = sha256d(payload);
    data.extend_from_slice(&checksum[..4]);
    base58_encode(&data)
}

fn base58_encode(data: &[u8]) -> String {
    let zeros = data.iter().take_while(|&&b| b == 0).count();
    let mut digits: Vec<u8> = Vec::new(); // little-endian base58 digits
    for &byte in data {
        let mut carry = u32::from(byte);
        for d in &mut digits {
            let v = u32::from(*d) * 256 + carry;
            *d = (v % 58) as u8;
            carry = v / 58;
        }
        while carry > 0 {
            digits.push((carry % 58) as u8);
            carry /= 58;
        }
    }
    let mut out = String::with_capacity(zeros + digits.len());
    for _ in 0..zeros {
        out.push('1');
    }
    for &d in digits.iter().rev() {
        out.push(B58[d as usize] as char);
    }
    out
}

fn base58_decode(s: &str) -> Result<Vec<u8>, String> {
    let zeros = s.chars().take_while(|&c| c == '1').count();
    let mut bytes: Vec<u8> = Vec::new(); // little-endian
    for ch in s.chars() {
        let idx = B58
            .iter()
            .position(|&c| c as char == ch)
            .ok_or_else(|| format!("bad base58 character '{ch}'"))? as u32;
        let mut carry = idx;
        for b in &mut bytes {
            let v = u32::from(*b) * 58 + carry;
            *b = (v % 256) as u8;
            carry = v / 256;
        }
        while carry > 0 {
            bytes.push((carry % 256) as u8);
            carry /= 256;
        }
    }
    let mut out = vec![0u8; zeros];
    out.extend(bytes.iter().rev());
    Ok(out)
}

/// Derive a stable pool authority keypair from a 32-byte secret (the
/// `--v2-authority-key` flag): `(x-only public, secret)`. The public key is
/// what miners pin (or the pool publishes as a §4.7 URL key).
///
/// # Errors
/// When the secret is not a valid secp256k1 scalar (all-zero or ≥ order).
pub fn authority_from_secret(secret: &[u8; 32]) -> Result<([u8; 32], [u8; 32]), String> {
    let secp = secp256k1::Secp256k1::new();
    let sk = secp256k1::SecretKey::from_slice(secret)
        .map_err(|e| format!("authority secret: {e}"))?;
    let kp = secp256k1::Keypair::from_secret_key(&secp, &sk);
    let (xonly, _parity) = kp.x_only_public_key();
    Ok((xonly.serialize(), kp.secret_bytes()))
}

/// 32 random bytes for an authority secret (`/dev/urandom`; a hashed
/// time/pid fallback keeps non-unix builds working — testnet-grade only,
/// and fatal on unix would be better).
fn random_secret() -> [u8; 32] {
    let mut key = [0u8; 32];
    #[cfg(unix)]
    {
        use std::io::Read as _;
        if std::fs::File::open("/dev/urandom")
            .and_then(|mut f| f.read_exact(&mut key))
            .is_ok()
        {
            return key;
        }
    }
    use sha2::{Digest, Sha256};
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mut msg = Vec::with_capacity(32);
    msg.extend_from_slice(b"lm-stratum-authority-v1");
    msg.extend_from_slice(&nanos.to_le_bytes());
    msg.extend_from_slice(&std::process::id().to_le_bytes());
    Sha256::digest(&msg).into()
}

/// A Noise-secured V2 session over any byte stream.
pub struct V2Session<S: Read + Write> {
    stream: S,
    encryptor: NoiseEncryptor,
    decryptor: NoiseDecryptor,
}

impl<S: Read + Write> V2Session<S> {
    /// Miner side: run the Noise NX handshake as the initiator.
    ///
    /// `authority` is the pool's x-only authority public key (32 bytes); when
    /// set, the responder certificate is verified against it. `None` accepts
    /// any responder (encrypted but unauthenticated — use only for local
    /// testing).
    ///
    /// # Errors
    /// [`SessionError::Handshake`] on a bad key or a failed certificate
    /// check; [`SessionError::Io`] on stream failures.
    pub fn connect(mut stream: S, authority: Option<[u8; 32]>) -> Result<Self, SessionError> {
        let pk = match authority {
            Some(a) => Some(
                XOnlyPublicKey::from_slice(&a)
                    .map_err(|e| SessionError::Handshake(format!("authority key: {e}")))?,
            ),
            None => None,
        };
        let mut initiator = Initiator::new(pk);
        let first = initiator
            .step_0()
            .map_err(|e| SessionError::Handshake(format!("step_0: {e:?}")))?;
        stream.write_all(&first)?;
        stream.flush()?;
        let mut reply = [0u8; HANDSHAKE_RESPONDER_LEN];
        stream.read_exact(&mut reply)?;
        let engine = initiator
            .step_2(reply)
            .map_err(|e| SessionError::Handshake(format!("step_2: {e:?}")))?;
        let (encryptor, decryptor) = engine.into_split();
        Ok(Self {
            stream,
            encryptor,
            decryptor,
        })
    }

    /// Pool side: run the handshake as the responder with a stable authority
    /// keypair ([`generate_authority`]) and a certificate validity.
    ///
    /// # Errors
    /// [`SessionError::Handshake`] on a bad authority keypair or a tampered
    /// initiator message; [`SessionError::Io`] on stream failures.
    pub fn accept(
        mut stream: S,
        authority: ([u8; 32], [u8; 32]),
        cert_validity: Duration,
    ) -> Result<Self, SessionError> {
        let (public, secret) = authority;
        let mut responder = Responder::from_authority_kp(&public, &secret, cert_validity)
            .map_err(|e| SessionError::Handshake(format!("authority keypair: {e:?}")))?;
        let mut first = [0u8; HANDSHAKE_INITIATOR_LEN];
        stream.read_exact(&mut first)?;
        let (reply, engine) = responder
            .step_1(first)
            .map_err(|e| SessionError::Handshake(format!("step_1: {e:?}")))?;
        stream.write_all(&reply)?;
        stream.flush()?;
        let (encryptor, decryptor) = engine.into_split();
        Ok(Self {
            stream,
            encryptor,
            decryptor,
        })
    }

    /// Encrypt and send one V2 frame: the header on its own, then the payload
    /// in 65519-byte plaintext blocks.
    ///
    /// # Errors
    /// [`SessionError::TooLong`] when the payload exceeds the frame cap;
    /// [`SessionError::Io`] on stream failures.
    pub fn send(&mut self, frame: &Frame) -> Result<(), SessionError> {
        if frame.payload.len() > MAX_FRAME_LEN {
            return Err(SessionError::TooLong(frame.payload.len()));
        }
        let mut out = Vec::with_capacity(NOISE_ENCRYPTED_HEADER_LEN + frame.payload.len() + MAC_LEN);
        let mut header = frame.header().to_vec();
        self.encryptor
            .encrypt(&mut header)
            .map_err(|_| SessionError::Decrypt)?;
        out.extend_from_slice(&header);
        for chunk in frame.payload.chunks(MAX_PLAINTEXT_CHUNK) {
            let mut block = chunk.to_vec();
            self.encryptor
                .encrypt(&mut block)
                .map_err(|_| SessionError::Decrypt)?;
            out.extend_from_slice(&block);
        }
        self.stream.write_all(&out)?;
        self.stream.flush()?;
        Ok(())
    }

    /// Receive one V2 frame, blocking until it has fully arrived.
    ///
    /// # Errors
    /// [`SessionError::Decrypt`] on a tampered block,
    /// [`SessionError::Frame`] on a malformed frame, [`SessionError::Io`] on
    /// stream failures.
    pub fn recv(&mut self) -> Result<Frame, SessionError> {
        let mut header = vec![0u8; NOISE_ENCRYPTED_HEADER_LEN];
        self.stream.read_exact(&mut header)?;
        self.decryptor
            .decrypt(&mut header)
            .map_err(|_| SessionError::Decrypt)?;
        header.truncate(NOISE_HEADER_LEN);
        let (extension_type, msg_type, plaintext_len) = Frame::parse_header(&header)?;
        let mut buf = vec![0u8; ciphertext_len(plaintext_len)];
        self.stream.read_exact(&mut buf)?;
        let mut payload = Vec::with_capacity(plaintext_len);
        let mut offset = 0;
        while offset < buf.len() {
            let n = (buf.len() - offset).min(MAX_CIPHERTEXT_CHUNK);
            let mut block = buf[offset..offset + n].to_vec();
            self.decryptor
                .decrypt(&mut block)
                .map_err(|_| SessionError::Decrypt)?;
            payload.extend_from_slice(&block);
            offset += n;
        }
        Ok(Frame {
            extension_type,
            msg_type,
            payload,
        })
    }
}

impl V2Session<std::net::TcpStream> {
    /// Read timeout control for the miner's poll loop (short timeouts make
    /// `recv` return a timeout error the caller treats as "no job yet").
    pub fn set_read_timeout(
        &self,
        dur: Option<std::time::Duration>,
    ) -> std::io::Result<()> {
        self.stream.set_read_timeout(dur)
    }
}

/// Miner side of version negotiation: send `SetupConnection` and read the
/// pool's reply.
///
/// # Errors
/// [`SessionError::Rejected`] when the pool answers with
/// `SetupConnection.Error`; session/codec errors otherwise.
pub fn setup_connection<S: Read + Write>(
    session: &mut V2Session<S>,
    setup: &SetupConnection,
) -> Result<SetupConnectionSuccess, SessionError> {
    session.send(&to_frame(setup))?;
    let frame = session.recv()?;
    match frame.msg_type {
        MSG_SETUP_CONNECTION_SUCCESS => {
            from_frame::<SetupConnectionSuccess>(&frame).map_err(SessionError::Codec)
        }
        MSG_SETUP_CONNECTION_ERROR => {
            let e = from_frame::<SetupConnectionError>(&frame).map_err(SessionError::Codec)?;
            Err(SessionError::Rejected(e.error_code))
        }
        other => Err(SessionError::UnexpectedMessage(other)),
    }
}

/// Pool side of version negotiation: read `SetupConnection` and answer with
/// `decide(&setup)` — `Ok(success)` accepts, `Err((flags, code))` sends
/// `SetupConnection.Error` and fails.
///
/// # Errors
/// [`SessionError::Rejected`] carries the code sent back to the miner; other
/// session/codec errors pass through.
pub fn accept_setup_connection<S: Read + Write>(
    session: &mut V2Session<S>,
    decide: impl FnOnce(&SetupConnection) -> Result<SetupConnectionSuccess, (u32, String)>,
) -> Result<SetupConnection, SessionError> {
    let frame = session.recv()?;
    if frame.msg_type != MSG_SETUP_CONNECTION {
        return Err(SessionError::UnexpectedMessage(frame.msg_type));
    }
    let setup = from_frame::<SetupConnection>(&frame).map_err(SessionError::Codec)?;
    match decide(&setup) {
        Ok(success) => {
            session.send(&to_frame(&success))?;
            Ok(setup)
        }
        Err((flags, code)) => {
            session.send(&to_frame(&SetupConnectionError {
                flags,
                error_code: code.clone(),
            }))?;
            Err(SessionError::Rejected(code))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::v2::PROTOCOL_MINING;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    /// In-memory duplex byte pipe with one-shot tampering: when armed, the
    /// next write has its first byte flipped (so the AEAD must reject it).
    #[derive(Clone)]
    struct Duplex {
        rx: Arc<Mutex<VecDeque<u8>>>,
        tx: Arc<Mutex<VecDeque<u8>>>,
        flip: Arc<AtomicBool>,
    }

    impl Duplex {
        fn pair() -> (Self, Self) {
            let a = Arc::new(Mutex::new(VecDeque::new()));
            let b = Arc::new(Mutex::new(VecDeque::new()));
            let flip = Arc::new(AtomicBool::new(false));
            (
                Self {
                    rx: Arc::clone(&a),
                    tx: Arc::clone(&b),
                    flip: Arc::clone(&flip),
                },
                Self {
                    rx: b,
                    tx: a,
                    flip,
                },
            )
        }
    }

    impl Read for Duplex {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                {
                    let mut q = self.rx.lock().expect("rx");
                    if !q.is_empty() {
                        let n = buf.len().min(q.len());
                        for (i, byte) in q.drain(..n).enumerate() {
                            buf[i] = byte;
                        }
                        return Ok(n);
                    }
                }
                if Instant::now() > deadline {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "duplex timeout",
                    ));
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }

    impl Write for Duplex {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let mut out = buf.to_vec();
            if !out.is_empty() && self.flip.swap(false, Ordering::SeqCst) {
                out[0] ^= 0x01;
            }
            self.tx.lock().expect("tx").extend(out);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn miner_setup() -> SetupConnection {
        SetupConnection {
            protocol: PROTOCOL_MINING,
            min_version: 2,
            max_version: 2,
            flags: 0,
            endpoint_host: "pool.example".to_string(),
            endpoint_port: 3346,
            vendor: "lm-miner".to_string(),
            hardware_version: "cpu".to_string(),
            firmware: "0.2".to_string(),
            device_id: "rig-1".to_string(),
        }
    }

    #[test]
    fn authority_from_secret_derives_the_pinned_key() {
        let secret = [0x11u8; 32];
        let (public, back) = authority_from_secret(&secret).expect("valid scalar");
        assert_eq!(back, secret);
        // The pool publishes the URL key; the miner parses it back.
        let url = authority_url_key(&public);
        assert_eq!(parse_authority_url_key(&url).expect("parse"), public);
        // Degenerate secrets are refused, never silently mapped.
        assert!(authority_from_secret(&[0u8; 32]).is_err());
        assert!(authority_from_secret(&[0xffu8; 32]).is_err());
    }

    #[test]
    fn authority_url_key_matches_the_spec_vector() {
        // sv2-spec §4.7.1 test vector.
        let raw: [u8; 32] = [
            118, 99, 112, 0, 151, 156, 28, 17, 175, 12, 48, 11, 205, 140, 127, 228, 134, 16, 252,
            233, 185, 193, 30, 61, 174, 227, 90, 224, 176, 138, 116, 85,
        ];
        let encoded = authority_url_key(&raw);
        assert_eq!(
            encoded, "9bXiEd8boQVhq7WddEcERUL5tyyJVFYdU8th3HfbNXK3Yw6GRXh",
            "must match the spec vector"
        );
        // Round trip.
        assert_eq!(parse_authority_url_key(&encoded).unwrap(), raw);
        // A tampered last character breaks the checksum.
        let mut bad = encoded.clone();
        bad.pop();
        bad.push('2');
        assert!(parse_authority_url_key(&bad).is_err());
        assert!(parse_authority_url_key("not!base58").is_err());
        // Generated keys round trip too.
        let (pubkey, _secret) = generate_authority();
        let url = authority_url_key(&pubkey);
        assert_eq!(parse_authority_url_key(&url).unwrap(), pubkey);
    }

    #[test]
    fn ciphertext_length_matches_the_spec_table() {
        // Spec §4.6: blocks of 65519 plaintext become 65535 ciphertext; a
        // partial tail block gains one 16-byte tag.
        assert_eq!(ciphertext_len(0), 0);
        assert_eq!(ciphertext_len(1), 17);
        assert_eq!(ciphertext_len(6), 22);
        assert_eq!(ciphertext_len(MAX_PLAINTEXT_CHUNK), MAX_CIPHERTEXT_CHUNK);
        assert_eq!(ciphertext_len(MAX_PLAINTEXT_CHUNK + 1), MAX_CIPHERTEXT_CHUNK + 17);
        assert_eq!(
            ciphertext_len(2 * MAX_PLAINTEXT_CHUNK),
            2 * MAX_CIPHERTEXT_CHUNK
        );
        assert_eq!(
            ciphertext_len(2 * MAX_PLAINTEXT_CHUNK + 5),
            2 * MAX_CIPHERTEXT_CHUNK + 21
        );
    }

    #[test]
    fn handshake_and_encrypted_version_negotiation_round_trip() {
        let authority = generate_authority();
        let (client, server) = Duplex::pair();
        let pubkey = authority.0;
        let server_thread = std::thread::spawn(move || {
            let mut s = V2Session::accept(server, authority, Duration::from_secs(3600))
                .expect("accept handshake");
            let setup = accept_setup_connection(&mut s, |setup| {
                assert_eq!(setup.vendor, "lm-miner");
                assert_eq!(setup.protocol, PROTOCOL_MINING);
                Ok(SetupConnectionSuccess {
                    used_version: setup.max_version.min(2),
                    flags: 0,
                })
            })
            .expect("negotiate");
            assert_eq!(setup.device_id, "rig-1");
            // One more frame each way, after negotiation.
            let f = s.recv().expect("recv ping");
            assert_eq!(f.msg_type, 0x7f);
            s.send(&Frame::new(0, 0x7e, false, vec![1, 2, 3]))
                .expect("send pong");
        });

        let mut c = V2Session::connect(client, Some(pubkey)).expect("connect handshake");
        let success = setup_connection(&mut c, &miner_setup()).expect("negotiate");
        assert_eq!(success.used_version, 2);
        c.send(&Frame::new(0, 0x7f, false, vec![9, 9]))
            .expect("send ping");
        let pong = c.recv().expect("recv pong");
        assert_eq!(pong.msg_type, 0x7e);
        assert_eq!(pong.payload, vec![1, 2, 3]);
        server_thread.join().expect("server thread");
    }

    #[test]
    fn multi_chunk_payloads_round_trip() {
        let authority = generate_authority();
        let (client, server) = Duplex::pair();
        // 70 KiB forces two ciphertext blocks (65519 + 4481).
        let big: Vec<u8> = (0..70_000).map(|i| (i % 251) as u8).collect();
        let expected = big.clone();
        let server_thread = std::thread::spawn(move || {
            let mut s = V2Session::accept(server, authority, Duration::from_secs(3600))
                .expect("accept handshake");
            let f = s.recv().expect("recv big");
            f.payload
        });
        let mut c = V2Session::connect(client, Some(authority.0)).expect("connect");
        c.send(&Frame::new(0, 0x21, false, big)).expect("send big");
        let got = server_thread.join().expect("thread");
        assert_eq!(got.len(), 70_000);
        assert_eq!(got, expected);
    }

    #[test]
    fn empty_payload_frames_round_trip() {
        let authority = generate_authority();
        let (client, server) = Duplex::pair();
        let server_thread = std::thread::spawn(move || {
            let mut s = V2Session::accept(server, authority, Duration::from_secs(3600))
                .expect("accept");
            s.recv().expect("recv empty")
        });
        let mut c = V2Session::connect(client, Some(authority.0)).expect("connect");
        c.send(&Frame::new(0, 0x04, false, Vec::new()))
            .expect("send empty");
        let f = server_thread.join().expect("thread");
        assert_eq!(f.msg_type, 0x04);
        assert!(f.payload.is_empty());
    }

    #[test]
    fn tampered_transport_block_is_rejected() {
        let authority = generate_authority();
        let (client, server) = Duplex::pair();
        let flip_handle = client.flip.clone();
        let server_thread = std::thread::spawn(move || {
            let mut s = V2Session::accept(server, authority, Duration::from_secs(3600))
                .expect("accept handshake");
            s.recv()
        });
        let mut c = V2Session::connect(client, Some(authority.0)).expect("connect handshake");
        // Arm the tamper: the next write gets one byte flipped (the encrypted
        // header in this framing).
        flip_handle.store(true, Ordering::SeqCst);
        c.send(&Frame::new(0, MSG_SETUP_CONNECTION, false, vec![0u8; 32]))
            .expect("local send");
        let result = server_thread.join().expect("server thread");
        assert!(
            matches!(result, Err(SessionError::Decrypt)),
            "tampered frame must fail authentication: {result:?}"
        );
    }

    #[test]
    fn wrong_authority_key_fails_the_handshake() {
        let authority = generate_authority();
        let other = generate_authority();
        let (client, server) = Duplex::pair();
        std::thread::spawn(move || {
            // The responder completes its side; the initiator must refuse it.
            let _ = V2Session::accept(server, authority, Duration::from_secs(3600));
        });
        let err = match V2Session::connect(client, Some(other.0)) {
            Ok(_) => panic!("a pinned key must reject a different authority"),
            Err(e) => e,
        };
        assert!(
            matches!(err, SessionError::Handshake(_)),
            "pinned key must reject a different authority: {err:?}"
        );
    }

    #[test]
    fn unpinned_initiator_accepts_any_responder() {
        let authority = generate_authority();
        let (client, server) = Duplex::pair();
        let server_thread = std::thread::spawn(move || {
            let mut s = V2Session::accept(server, authority, Duration::from_secs(3600))
                .expect("accept handshake");
            s.recv().expect("recv")
        });
        let mut c = V2Session::connect(client, None).expect("unpinned connect");
        c.send(&Frame::new(0, 0x7f, false, vec![7])).expect("send");
        let f = server_thread.join().expect("thread");
        assert_eq!(f.msg_type, 0x7f);
    }

    #[test]
    fn interop_with_reference_codec_both_directions() {
        use codec_sv2::{Decrypted, Handshake, NoiseDecoder, NoiseEncoder};
        use framing_sv2::framing::SerializedFrame as RefFrame;
        use framing_sv2::header::Header as RefHeader;
        use noise_sv2::Initiator;
        use std::io::{Read, Write};

        // Our pool side accepts; a reference-initiator drives the handshake
        // (both speak noise_sv2 2.0.0, so the transcript must agree).
        let authority = generate_authority();
        let (mut io, server) = Duplex::pair();
        let xonly =
            secp256k1::XOnlyPublicKey::from_slice(&authority.0).expect("authority xonly");
        let (msg0, hs) = Handshake::initiator(Initiator::new(Some(xonly)))
            .step_0()
            .expect("ref step_0");
        // No threads needed: the duplex buffers decouple the two sides.
        io.write_all(msg0.payload()).expect("write msg0");
        let mut ours =
            V2Session::accept(server, authority, Duration::from_secs(3600)).expect("our accept");
        let mut reply = [0u8; HANDSHAKE_RESPONDER_LEN];
        io.read_exact(&mut reply).expect("read reply");
        let transport = hs.step_2(reply).expect("ref step_2");
        let (mut ref_enc, ref_dec) = transport.split();

        // Real Lucky messages, plus a multi-chunk payload and an empty one.
        let frames = vec![
            to_frame(&crate::v2::LuckyJob {
                channel_id: 1,
                job_id: 9,
                height: 1234,
                header: vec![0xabu8; 109],
                pool_target: [0x01; 32],
                net_target: [0x00; 32],
                clean: true,
            }),
            to_frame(&crate::v2::LuckySubmit {
                channel_id: 1,
                job_id: 9,
                sequence_number: 3,
                nonce: 0xdead_beef_1234_5678,
            }),
            Frame::new(0, 0x21, true, (0..70_000).map(|i| (i % 251) as u8).collect()),
            Frame::new(0, 0x04, false, Vec::new()),
        ];

        // Length-table equivalence against THEIR function, per frame.
        for f in &frames {
            let rh = RefHeader::from_bytes(&f.header()).expect("their header parses ours");
            assert_eq!(
                ciphertext_len(f.payload.len()),
                codec_sv2::encrypted_payload_length(&rh),
                "length table must agree"
            );
        }

        // Ours -> reference decoder.
        let mut decoder = NoiseDecoder::new();
        let mut dec_state = ref_dec;
        for f in &frames {
            ours.send(f).expect("our send");
            loop {
                match decoder.next_transport_frame(dec_state) {
                    Ok(Decrypted::Frame(sf, s)) => {
                        dec_state = s;
                        assert_eq!(sf.as_bytes(), &f.encode());
                        break;
                    }
                    Ok(Decrypted::Incomplete(_, s)) => {
                        dec_state = s;
                        let want = decoder.writable_len();
                        let mut buf = vec![0u8; want];
                        io.read_exact(&mut buf).expect("read more ct");
                        decoder.writable()[..want].copy_from_slice(&buf);
                    }
                    Err(e) => panic!("reference failed to decode ours: {e:?}"),
                }
            }
        }

        // Reference encoder -> ours.
        let mut encoder = NoiseEncoder::new();
        for f in &frames {
            let plain = RefFrame::from_bytes(f.encode()).expect("their frame from our bytes");
            let slice = encoder
                .encode_transport(plain, &mut ref_enc)
                .expect("ref encode");
            io.write_all(slice.as_ref()).expect("write ref ct");
            let got = ours.recv().expect("our recv of theirs");
            assert_eq!(got, *f);
        }

        // Tampered bytes fail reference-side authentication (last, since a
        // failed round consumes the state).
        io.flip.store(true, Ordering::SeqCst);
        ours.send(&frames[0]).expect("send for tamper");
        loop {
            match decoder.next_transport_frame(dec_state) {
                Ok(Decrypted::Frame(..)) => panic!("tampered bytes must not decode"),
                Ok(Decrypted::Incomplete(_, s)) => {
                    dec_state = s;
                    let want = decoder.writable_len();
                    let mut buf = vec![0u8; want];
                    io.read_exact(&mut buf).expect("read tampered ct");
                    decoder.writable()[..want].copy_from_slice(&buf);
                }
                Err(_) => break,
            }
        }
    }

    #[test]
    fn oversized_frames_are_refused_before_encryption() {
        let authority = generate_authority();
        let (client, server) = Duplex::pair();
        std::thread::spawn(move || {
            let _ = V2Session::accept(server, authority, Duration::from_secs(3600));
        });
        let mut c = V2Session::connect(client, Some(authority.0)).expect("connect");
        let big = Frame::new(0, 0x01, false, vec![0u8; MAX_FRAME_LEN + 1]);
        let err = c.send(&big).unwrap_err();
        assert!(matches!(err, SessionError::TooLong(_)), "{err:?}");
    }

    /// The whole V2 core stack over one secure session: handshake →
    /// SetupConnection → OpenStandardMiningChannel → NewMiningJob →
    /// SubmitSharesStandard → SubmitSharesSuccess, every message encrypted.
    #[test]
    fn full_v2_core_flow_over_a_secure_session() {
        use crate::v2::{
            NewMiningJob, OpenStandardMiningChannel, OpenStandardMiningChannelSuccess,
            SubmitSharesStandard, SubmitSharesSuccess,
        };
        let authority = generate_authority();
        let (client, server) = Duplex::pair();
        let server_thread = std::thread::spawn(move || {
            let mut s =
                V2Session::accept(server, authority, Duration::from_secs(3600)).expect("accept");
            accept_setup_connection(&mut s, |_| {
                Ok(SetupConnectionSuccess {
                    used_version: 2,
                    flags: 0,
                })
            })
            .expect("negotiate");
            let open = from_frame::<OpenStandardMiningChannel>(&s.recv().expect("recv open"))
                .expect("decode open");
            assert_eq!(open.user_identity, "rig-1");
            s.send(&to_frame(&OpenStandardMiningChannelSuccess {
                request_id: open.request_id,
                channel_id: 1,
                target: [0xff; 32],
                extranonce_prefix: vec![0xaa, 0xbb],
                group_channel_id: 0,
            }))
            .expect("send open success");
            s.send(&to_frame(&NewMiningJob {
                channel_id: 1,
                job_id: 7,
                ntime_start: None,
                version: 0x2000_0000,
                merkle_root: [0x11; 32],
            }))
            .expect("send job");
            let share = from_frame::<SubmitSharesStandard>(&s.recv().expect("recv share"))
                .expect("decode share");
            assert_eq!(share.sequence_number, 1);
            assert_eq!(share.nonce, 12345);
            s.send(&to_frame(&SubmitSharesSuccess {
                channel_id: 1,
                last_sequence_number: share.sequence_number,
                new_submits_accepted_count: 1,
                new_shares_sum: 1,
            }))
            .expect("send share success");
        });

        let mut c = V2Session::connect(client, Some(authority.0)).expect("connect");
        let ok = setup_connection(&mut c, &miner_setup()).expect("negotiate");
        assert_eq!(ok.used_version, 2);
        c.send(&to_frame(&OpenStandardMiningChannel {
            request_id: 42,
            user_identity: "rig-1".to_string(),
            nominal_hash_rate: 1_000.0,
            max_target: [0xff; 32],
        }))
        .expect("send open");
        let opened =
            from_frame::<OpenStandardMiningChannelSuccess>(&c.recv().expect("recv open success"))
                .expect("decode open success");
        assert_eq!(opened.channel_id, 1);
        assert_eq!(opened.extranonce_prefix, vec![0xaa, 0xbb]);
        let job = from_frame::<NewMiningJob>(&c.recv().expect("recv job")).expect("decode job");
        assert_eq!(job.job_id, 7);
        assert_eq!(job.merkle_root, [0x11; 32]);
        c.send(&to_frame(&SubmitSharesStandard {
            channel_id: 1,
            sequence_number: 1,
            job_id: 7,
            nonce: 12345,
            ntime: 0,
            version: 0x2000_0000,
        }))
        .expect("send share");
        let accepted =
            from_frame::<SubmitSharesSuccess>(&c.recv().expect("recv share success"))
                .expect("decode share success");
        assert_eq!(accepted.new_submits_accepted_count, 1);
        assert_eq!(accepted.last_sequence_number, 1);
        server_thread.join().expect("server thread");
    }
}
