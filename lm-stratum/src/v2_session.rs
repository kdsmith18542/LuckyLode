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

/// 32 random bytes for an authority secret (`/dev/urandom`; a hashed
/// time/pid fallback keeps non-unix builds working — testnet-grade, the same
/// posture as the casino's seed generator).
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
