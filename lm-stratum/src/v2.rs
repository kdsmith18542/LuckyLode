//! Stratum V2 framing + core message codec (the "framing" milestone; V1
//! remains the live wire — this module is the framed layer a later V2
//! negotiation will use, tested standalone).
//!
//! Spec: sv2-spec `03-Protocol-Overview.md` §3.2 (framing) / §3.6
//! (common messages), `05-Mining-Protocol.md` §5.3 (mining messages).
//! Message-type numbers and channel bits match the reference
//! implementation (`common_messages_sv2` / `mining_sv2` constants), so a
//! future interop test can diff against it directly.
//!
//! Frame layout (6-byte header, all integers little-endian):
//!
//! ```text
//! extension_type: U16 LE   (0x0000 = core spec messages)
//! msg_type:       U8
//! msg_length:     U24 LE   (payload length, header excluded)
//! payload:        BYTES
//! ```
//!
//! Bit 15 of `extension_type` (`channel_msg`) marks channel-scoped
//! messages; their payload starts with a `U32` channel_id (included in
//! `msg_length`).
//!
//! **Not implemented here** (later milestones): Noise NX handshake and
//! AEAD (the real V2 is always encrypted), TLV extension fields, and the
//! job-declaration / template-distribution subprotocols. Unknown
//! extension types decode at the frame layer (the spec says to route or
//! ignore them) — typed decoding only accepts core messages.

use std::fmt;

/// Core-spec extension type (common + mining + JD + template messages).
pub const EXTENSION_TYPE_CORE: u16 = 0x0000;
/// Bit 15 of `extension_type`: message is channel-scoped.
pub const CHANNEL_MSG_BIT: u16 = 0x8000;
/// Sanity guard for `msg_length` (spec allows U24; we refuse > 1 MiB).
pub const MAX_FRAME_LEN: usize = 1 << 20;

// Common protocol messages (sv2 `common_messages_sv2`).
pub const MSG_SETUP_CONNECTION: u8 = 0x00;
pub const MSG_SETUP_CONNECTION_SUCCESS: u8 = 0x01;
pub const MSG_SETUP_CONNECTION_ERROR: u8 = 0x02;
pub const MSG_RECONNECT: u8 = 0x04;

// Mining protocol messages (sv2 `mining_sv2`).
pub const MSG_OPEN_STANDARD_MINING_CHANNEL: u8 = 0x10;
pub const MSG_OPEN_STANDARD_MINING_CHANNEL_SUCCESS: u8 = 0x11;
pub const MSG_OPEN_MINING_CHANNEL_ERROR: u8 = 0x12;
pub const MSG_NEW_MINING_JOB: u8 = 0x15;
pub const MSG_CLOSE_CHANNEL: u8 = 0x18;
pub const MSG_SUBMIT_SHARES_STANDARD: u8 = 0x1a;
pub const MSG_SUBMIT_SHARES_SUCCESS: u8 = 0x1c;
pub const MSG_SUBMIT_SHARES_ERROR: u8 = 0x1d;
pub const MSG_SET_NEW_PREV_HASH: u8 = 0x20;
pub const MSG_SET_TARGET: u8 = 0x21;

/// Lucky Lode extension type (`"LL"`): core messages carry the Noise
/// handshake and channel open/acks, but the core `NewMiningJob` (merkle +
/// version + ntime) cannot express a LuckyHash template (opaque 109-byte
/// header, height-bound salt, u64 nonce domain). Extension jobs carry the
/// full template; shares carry the u64 nonce. Acks reuse the core
/// `SubmitShares.Success/Error` types.
pub const EXTENSION_LUCKY: u16 = 0x4C4C;

/// `SetupConnectionSuccess.flags` bit: this pool serves the Lucky
/// extension (0x4C4C — `LuckyJob`/`LuckySubmit`). A conforming V2 client
/// that only implements the core messages sees flag 0 and learns it
/// must not expect core `NewMiningJob` work here; previously the pool
/// advertised nothing, so a strict client silently got no work.
pub const FLAG_LUCKY_EXTENSION: u32 = 0x1;
/// Lucky job: full header template for a channel (extension message).
pub const MSG_LUCKY_JOB: u8 = 0x00;
/// Lucky share submit with the u64 grind nonce (extension message).
pub const MSG_LUCKY_SUBMIT: u8 = 0x01;

/// Protocol discriminant for `SetupConnection.protocol` (mining).
pub const PROTOCOL_MINING: u8 = 0;

/// Framing errors. Typed decoding has its own [`CodecError`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    /// A frame header claimed a payload larger than [`MAX_FRAME_LEN`].
    PayloadTooLarge(usize),
    /// Fewer than [`Frame::HEADER_LEN`] bytes were available for a header.
    ShortHeader,
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FrameError::PayloadTooLarge(n) => write!(f, "frame payload {n} exceeds cap"),
            FrameError::ShortHeader => write!(f, "short frame header"),
        }
    }
}

/// One Stratum V2 frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub extension_type: u16,
    pub msg_type: u8,
    pub payload: Vec<u8>,
}

impl Frame {
    /// Frame header length: extension `U16` LE, message type `U8`, payload
    /// length `U24` LE.
    pub const HEADER_LEN: usize = 6;

    #[must_use]
    pub fn new(extension_type: u16, msg_type: u8, channel_msg: bool, payload: Vec<u8>) -> Self {
        let extension_type = if channel_msg {
            extension_type | CHANNEL_MSG_BIT
        } else {
            extension_type & !CHANNEL_MSG_BIT
        };
        Self {
            extension_type,
            msg_type,
            payload,
        }
    }

    /// Channel-scoped (payload starts with `channel_id`)?
    #[must_use]
    pub fn channel_msg(&self) -> bool {
        self.extension_type & CHANNEL_MSG_BIT != 0
    }

    /// Extension id with the `channel_msg` bit masked off.
    #[must_use]
    pub fn extension(&self) -> u16 {
        self.extension_type & !CHANNEL_MSG_BIT
    }

    /// The 6-byte header (extension with the channel bit, type, `U24`
    /// payload length).
    #[must_use]
    pub fn header(&self) -> [u8; Self::HEADER_LEN] {
        let n = self.payload.len() as u32;
        [
            (self.extension_type & 0xff) as u8,
            (self.extension_type >> 8) as u8,
            self.msg_type,
            (n & 0xff) as u8,
            ((n >> 8) & 0xff) as u8,
            ((n >> 16) & 0xff) as u8,
        ]
    }

    /// Parse a 6-byte header into `(extension_type, msg_type, payload_len)`.
    ///
    /// # Errors
    /// [`FrameError::ShortHeader`] / [`FrameError::PayloadTooLarge`].
    pub fn parse_header(bytes: &[u8]) -> Result<(u16, u8, usize), FrameError> {
        if bytes.len() < Self::HEADER_LEN {
            return Err(FrameError::ShortHeader);
        }
        let extension_type = u16::from_le_bytes([bytes[0], bytes[1]]);
        let msg_type = bytes[2];
        let len = (bytes[3] as usize) | ((bytes[4] as usize) << 8) | ((bytes[5] as usize) << 16);
        if len > MAX_FRAME_LEN {
            return Err(FrameError::PayloadTooLarge(len));
        }
        Ok((extension_type, msg_type, len))
    }

    /// Header + payload bytes.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(Self::HEADER_LEN + self.payload.len());
        out.extend_from_slice(&self.header());
        out.extend_from_slice(&self.payload);
        out
    }

    /// Decode one frame from the front of `buf`. `Ok(None)` = the buffer
    /// does not yet hold a complete frame (streaming-friendly).
    pub fn decode(buf: &[u8]) -> Result<Option<(Self, usize)>, FrameError> {
        if buf.len() < Self::HEADER_LEN {
            return Ok(None);
        }
        let (extension_type, msg_type, len) = Self::parse_header(buf)?;
        if buf.len() < Self::HEADER_LEN + len {
            return Ok(None);
        }
        let payload = buf[Self::HEADER_LEN..Self::HEADER_LEN + len].to_vec();
        Ok(Some((
            Self {
                extension_type,
                msg_type,
                payload,
            },
            Self::HEADER_LEN + len,
        )))
    }
}

/// Streaming frame reader: feed arbitrary chunks (TCP reads split
/// anywhere), pull whole frames. Bounded: refuses to buffer past
/// [`MAX_FRAME_LEN`] + header for one frame.
#[derive(Debug, Default)]
pub struct FrameReader {
    buf: Vec<u8>,
}

impl FrameReader {
    #[must_use]
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Next complete frame, or `None` when more bytes are needed.
    pub fn next_frame(&mut self) -> Result<Option<Frame>, FrameError> {
        match Frame::decode(&self.buf)? {
            Some((frame, used)) => {
                self.buf.drain(..used);
                Ok(Some(frame))
            }
            None => {
                // Guard unbounded growth if a header is stuck mid-arrival.
                if self.buf.len() > MAX_FRAME_LEN + 6 {
                    return Err(FrameError::PayloadTooLarge(self.buf.len()));
                }
                Ok(None)
            }
        }
    }

    /// Bytes buffered but not yet consumed.
    #[must_use]
    pub fn buffered(&self) -> usize {
        self.buf.len()
    }
}

/// Typed-message codec errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodecError {
    /// Ran out of bytes mid-field.
    UnexpectedEof(&'static str),
    /// A length-prefixed field was longer than its type allows.
    LengthOutOfRange(&'static str, usize),
    /// Bytes remained after the message's fields.
    TrailingBytes(usize),
    /// A string field was not valid UTF-8.
    InvalidUtf8(&'static str),
    /// A value the spec forbids (NaN/inf/negative hash rate).
    InvalidValue(&'static str),
    /// Frame is not a core-spec message.
    WrongExtension(u16),
    /// Frame's message type differs from the type being decoded.
    WrongType(u8),
}

impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CodecError::UnexpectedEof(field) => write!(f, "eof in {field}"),
            CodecError::LengthOutOfRange(field, n) => write!(f, "{field} length {n} out of range"),
            CodecError::TrailingBytes(n) => write!(f, "{n} trailing bytes"),
            CodecError::InvalidUtf8(field) => write!(f, "{field} is not utf-8"),
            CodecError::InvalidValue(field) => write!(f, "invalid {field}"),
            CodecError::WrongExtension(e) => write!(f, "not a core frame (extension {e:#06x})"),
            CodecError::WrongType(t) => write!(f, "wrong message type {t:#04x}"),
        }
    }
}

// ---- little-endian primitive writer/reader (sv2-spec §3.1) ----

#[derive(Debug, Default)]
pub(crate) struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    fn new() -> Self {
        Self::default()
    }
    fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    fn f32(&mut self, v: f32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    fn u256(&mut self, v: &[u8; 32]) {
        self.buf.extend_from_slice(v);
    }
    fn str0_255(&mut self, s: &str) {
        let b = s.as_bytes();
        self.u8(b.len().min(255) as u8);
        self.buf.extend_from_slice(&b[..b.len().min(255)]);
    }
    fn bytes0_255(&mut self, b: &[u8]) {
        self.u8(b.len().min(255) as u8);
        self.buf.extend_from_slice(&b[..b.len().min(255)]);
    }
    fn bytes_u16(&mut self, b: &[u8]) {
        assert!(b.len() <= u16::MAX as usize, "bytes_u16 too long");
        self.u16(b.len() as u16);
        self.buf.extend_from_slice(b);
    }
    fn option_u32(&mut self, v: Option<u32>) {
        match v {
            Some(x) => {
                self.u8(1);
                self.u32(x);
            }
            None => self.u8(0),
        }
    }
}

#[derive(Debug)]
pub(crate) struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }
    fn take(&mut self, n: usize, field: &'static str) -> Result<&'a [u8], CodecError> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or(CodecError::UnexpectedEof(field))?;
        if end > self.buf.len() {
            return Err(CodecError::UnexpectedEof(field));
        }
        let out = &self.buf[self.pos..end];
        self.pos = end;
        Ok(out)
    }
    fn u8(&mut self, field: &'static str) -> Result<u8, CodecError> {
        Ok(self.take(1, field)?[0])
    }
    fn u16(&mut self, field: &'static str) -> Result<u16, CodecError> {
        let b = self.take(2, field)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }
    fn u32(&mut self, field: &'static str) -> Result<u32, CodecError> {
        let b = self.take(4, field)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn u64(&mut self, field: &'static str) -> Result<u64, CodecError> {
        let b = self.take(8, field)?;
        Ok(u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }
    fn f32(&mut self, field: &'static str) -> Result<f32, CodecError> {
        let b = self.take(4, field)?;
        Ok(f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn u256(&mut self, field: &'static str) -> Result<[u8; 32], CodecError> {
        let b = self.take(32, field)?;
        b.try_into().map_err(|_| CodecError::UnexpectedEof(field))
    }
    fn str0_255(&mut self, field: &'static str) -> Result<String, CodecError> {
        let n = self.u8(field)? as usize;
        let b = self.take(n, field)?;
        std::str::from_utf8(b)
            .map(str::to_string)
            .map_err(|_| CodecError::InvalidUtf8(field))
    }
    fn bytes0_255(&mut self, field: &'static str) -> Result<Vec<u8>, CodecError> {
        let n = self.u8(field)? as usize;
        if n > 32 {
            return Err(CodecError::LengthOutOfRange(field, n));
        }
        Ok(self.take(n, field)?.to_vec())
    }
    fn bytes_u16(&mut self, field: &'static str) -> Result<Vec<u8>, CodecError> {
        let n = self.u16(field)? as usize;
        Ok(self.take(n, field)?.to_vec())
    }
    fn option_u32(&mut self, field: &'static str) -> Result<Option<u32>, CodecError> {
        match self.u8(field)? {
            0 => Ok(None),
            1 => Ok(Some(self.u32(field)?)),
            _ => Err(CodecError::InvalidValue(field)),
        }
    }
    fn done(&self) -> Result<(), CodecError> {
        let left = self.buf.len() - self.pos;
        if left > 0 {
            return Err(CodecError::TrailingBytes(left));
        }
        Ok(())
    }
}

/// A typed protocol message. `EXTENSION` selects the namespace (core spec
/// vs Lucky extension); `CHANNEL_MSG` sets the channel bit.
pub(crate) trait Message: Sized {
    const EXTENSION: u16;
    const MSG_TYPE: u8;
    const CHANNEL_MSG: bool;
    fn encode_payload(&self, w: &mut Writer);
    fn decode_payload(r: &mut Reader<'_>) -> Result<Self, CodecError>;
}

pub(crate) fn to_frame<M: Message>(m: &M) -> Frame {
    let mut w = Writer::new();
    m.encode_payload(&mut w);
    Frame::new(M::EXTENSION, M::MSG_TYPE, M::CHANNEL_MSG, w.buf)
}

pub(crate) fn from_frame<M: Message>(f: &Frame) -> Result<M, CodecError> {
    if f.extension() != M::EXTENSION {
        return Err(CodecError::WrongExtension(f.extension()));
    }
    if f.msg_type != M::MSG_TYPE {
        return Err(CodecError::WrongType(f.msg_type));
    }
    let mut r = Reader::new(&f.payload);
    let m = M::decode_payload(&mut r)?;
    r.done()?;
    Ok(m)
}

// ---- common messages (§3.6) ----

/// §3.6.1 — the first message on a connection.
#[derive(Debug, Clone, PartialEq)]
pub struct SetupConnection {
    pub protocol: u8,
    pub min_version: u16,
    pub max_version: u16,
    pub flags: u32,
    pub endpoint_host: String,
    pub endpoint_port: u16,
    pub vendor: String,
    pub hardware_version: String,
    pub firmware: String,
    pub device_id: String,
}

impl Message for SetupConnection {
    const EXTENSION: u16 = EXTENSION_TYPE_CORE;
    const MSG_TYPE: u8 = MSG_SETUP_CONNECTION;
    const CHANNEL_MSG: bool = false;
    fn encode_payload(&self, w: &mut Writer) {
        w.u8(self.protocol);
        w.u16(self.min_version);
        w.u16(self.max_version);
        w.u32(self.flags);
        w.str0_255(&self.endpoint_host);
        w.u16(self.endpoint_port);
        w.str0_255(&self.vendor);
        w.str0_255(&self.hardware_version);
        w.str0_255(&self.firmware);
        w.str0_255(&self.device_id);
    }
    fn decode_payload(r: &mut Reader<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            protocol: r.u8("protocol")?,
            min_version: r.u16("min_version")?,
            max_version: r.u16("max_version")?,
            flags: r.u32("flags")?,
            endpoint_host: r.str0_255("endpoint_host")?,
            endpoint_port: r.u16("endpoint_port")?,
            vendor: r.str0_255("vendor")?,
            hardware_version: r.str0_255("hardware_version")?,
            firmware: r.str0_255("firmware")?,
            device_id: r.str0_255("device_id")?,
        })
    }
}

/// §3.6.2 — server accepted the connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SetupConnectionSuccess {
    pub used_version: u16,
    pub flags: u32,
}

impl Message for SetupConnectionSuccess {
    const EXTENSION: u16 = EXTENSION_TYPE_CORE;
    const MSG_TYPE: u8 = MSG_SETUP_CONNECTION_SUCCESS;
    const CHANNEL_MSG: bool = false;
    fn encode_payload(&self, w: &mut Writer) {
        w.u16(self.used_version);
        w.u32(self.flags);
    }
    fn decode_payload(r: &mut Reader<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            used_version: r.u16("used_version")?,
            flags: r.u32("flags")?,
        })
    }
}

/// §3.6.3 — negotiation failed (flags = offending features, 0 = other).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupConnectionError {
    pub flags: u32,
    pub error_code: String,
}

impl Message for SetupConnectionError {
    const EXTENSION: u16 = EXTENSION_TYPE_CORE;
    const MSG_TYPE: u8 = MSG_SETUP_CONNECTION_ERROR;
    const CHANNEL_MSG: bool = false;
    fn encode_payload(&self, w: &mut Writer) {
        w.u32(self.flags);
        w.str0_255(&self.error_code);
    }
    fn decode_payload(r: &mut Reader<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            flags: r.u32("flags")?,
            error_code: r.str0_255("error_code")?,
        })
    }
}

// ---- mining messages (§5.3) ----

/// §5.3.2 — request a standard channel.
#[derive(Debug, Clone, PartialEq)]
pub struct OpenStandardMiningChannel {
    pub request_id: u32,
    pub user_identity: String,
    pub nominal_hash_rate: f32,
    pub max_target: [u8; 32],
}

impl Message for OpenStandardMiningChannel {
    const EXTENSION: u16 = EXTENSION_TYPE_CORE;
    const MSG_TYPE: u8 = MSG_OPEN_STANDARD_MINING_CHANNEL;
    const CHANNEL_MSG: bool = false;
    fn encode_payload(&self, w: &mut Writer) {
        w.u32(self.request_id);
        w.str0_255(&self.user_identity);
        w.f32(self.nominal_hash_rate);
        w.u256(&self.max_target);
    }
    fn decode_payload(r: &mut Reader<'_>) -> Result<Self, CodecError> {
        let request_id = r.u32("request_id")?;
        let user_identity = r.str0_255("user_identity")?;
        let nominal_hash_rate = r.f32("nominal_hash_rate")?;
        if !nominal_hash_rate.is_finite() || nominal_hash_rate < 0.0 {
            return Err(CodecError::InvalidValue("nominal_hash_rate"));
        }
        Ok(Self {
            request_id,
            user_identity,
            nominal_hash_rate,
            max_target: r.u256("max_target")?,
        })
    }
}

/// §5.3.3 — channel opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenStandardMiningChannelSuccess {
    pub request_id: u32,
    pub channel_id: u32,
    pub target: [u8; 32],
    pub extranonce_prefix: Vec<u8>,
    pub group_channel_id: u32,
}

impl Message for OpenStandardMiningChannelSuccess {
    const EXTENSION: u16 = EXTENSION_TYPE_CORE;
    const MSG_TYPE: u8 = MSG_OPEN_STANDARD_MINING_CHANNEL_SUCCESS;
    const CHANNEL_MSG: bool = false;
    fn encode_payload(&self, w: &mut Writer) {
        w.u32(self.request_id);
        w.u32(self.channel_id);
        w.u256(&self.target);
        w.bytes0_255(&self.extranonce_prefix);
        w.u32(self.group_channel_id);
    }
    fn decode_payload(r: &mut Reader<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            request_id: r.u32("request_id")?,
            channel_id: r.u32("channel_id")?,
            target: r.u256("target")?,
            extranonce_prefix: r.bytes0_255("extranonce_prefix")?,
            group_channel_id: r.u32("group_channel_id")?,
        })
    }
}

/// §5.3.11 — share submission (channel-scoped).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubmitSharesStandard {
    pub channel_id: u32,
    pub sequence_number: u32,
    pub job_id: u32,
    pub nonce: u32,
    pub ntime: u32,
    pub version: u32,
}

impl Message for SubmitSharesStandard {
    const EXTENSION: u16 = EXTENSION_TYPE_CORE;
    const MSG_TYPE: u8 = MSG_SUBMIT_SHARES_STANDARD;
    const CHANNEL_MSG: bool = true;
    fn encode_payload(&self, w: &mut Writer) {
        w.u32(self.channel_id);
        w.u32(self.sequence_number);
        w.u32(self.job_id);
        w.u32(self.nonce);
        w.u32(self.ntime);
        w.u32(self.version);
    }
    fn decode_payload(r: &mut Reader<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            channel_id: r.u32("channel_id")?,
            sequence_number: r.u32("sequence_number")?,
            job_id: r.u32("job_id")?,
            nonce: r.u32("nonce")?,
            ntime: r.u32("ntime")?,
            version: r.u32("version")?,
        })
    }
}

/// §5.3.13 — batched share acceptance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubmitSharesSuccess {
    pub channel_id: u32,
    pub last_sequence_number: u32,
    pub new_submits_accepted_count: u32,
    pub new_shares_sum: u64,
}

impl Message for SubmitSharesSuccess {
    const EXTENSION: u16 = EXTENSION_TYPE_CORE;
    const MSG_TYPE: u8 = MSG_SUBMIT_SHARES_SUCCESS;
    const CHANNEL_MSG: bool = true;
    fn encode_payload(&self, w: &mut Writer) {
        w.u32(self.channel_id);
        w.u32(self.last_sequence_number);
        w.u32(self.new_submits_accepted_count);
        w.u64(self.new_shares_sum);
    }
    fn decode_payload(r: &mut Reader<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            channel_id: r.u32("channel_id")?,
            last_sequence_number: r.u32("last_sequence_number")?,
            new_submits_accepted_count: r.u32("new_submits_accepted_count")?,
            new_shares_sum: r.u64("new_shares_sum")?,
        })
    }
}

/// §5.3.14 — share rejection (per submit).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmitSharesError {
    pub channel_id: u32,
    pub sequence_number: u32,
    pub error_code: String,
}

impl Message for SubmitSharesError {
    const EXTENSION: u16 = EXTENSION_TYPE_CORE;
    const MSG_TYPE: u8 = MSG_SUBMIT_SHARES_ERROR;
    const CHANNEL_MSG: bool = true;
    fn encode_payload(&self, w: &mut Writer) {
        w.u32(self.channel_id);
        w.u32(self.sequence_number);
        w.str0_255(&self.error_code);
    }
    fn decode_payload(r: &mut Reader<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            channel_id: r.u32("channel_id")?,
            sequence_number: r.u32("sequence_number")?,
            error_code: r.str0_255("error_code")?,
        })
    }
}

/// §5.3.15 — standard job (header-only mining; channel-scoped).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NewMiningJob {
    pub channel_id: u32,
    pub job_id: u32,
    /// `None` = future job (activated by `SetNewPrevHash`).
    pub ntime_start: Option<u32>,
    pub version: u32,
    pub merkle_root: [u8; 32],
}

impl Message for NewMiningJob {
    const EXTENSION: u16 = EXTENSION_TYPE_CORE;
    const MSG_TYPE: u8 = MSG_NEW_MINING_JOB;
    const CHANNEL_MSG: bool = true;
    fn encode_payload(&self, w: &mut Writer) {
        w.u32(self.channel_id);
        w.u32(self.job_id);
        w.option_u32(self.ntime_start);
        w.u32(self.version);
        w.u256(&self.merkle_root);
    }
    fn decode_payload(r: &mut Reader<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            channel_id: r.u32("channel_id")?,
            job_id: r.u32("job_id")?,
            ntime_start: r.option_u32("ntime_start")?,
            version: r.u32("version")?,
            merkle_root: r.u256("merkle_root")?,
        })
    }
}

/// §5.3.17 — new chain tip (channel-scoped broadcast).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SetNewPrevHash {
    pub channel_id: u32,
    pub job_id: u32,
    pub prev_hash: [u8; 32],
    pub ntime: u32,
}

impl Message for SetNewPrevHash {
    const EXTENSION: u16 = EXTENSION_TYPE_CORE;
    const MSG_TYPE: u8 = MSG_SET_NEW_PREV_HASH;
    const CHANNEL_MSG: bool = true;
    fn encode_payload(&self, w: &mut Writer) {
        w.u32(self.channel_id);
        w.u32(self.job_id);
        w.u256(&self.prev_hash);
        w.u32(self.ntime);
    }
    fn decode_payload(r: &mut Reader<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            channel_id: r.u32("channel_id")?,
            job_id: r.u32("job_id")?,
            prev_hash: r.u256("prev_hash")?,
            ntime: r.u32("ntime")?,
        })
    }
}

/// §5.3.21 — target update (channel-scoped).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SetTarget {
    pub channel_id: u32,
    pub maximum_target: [u8; 32],
}

impl Message for SetTarget {
    const EXTENSION: u16 = EXTENSION_TYPE_CORE;
    const MSG_TYPE: u8 = MSG_SET_TARGET;
    const CHANNEL_MSG: bool = true;
    fn encode_payload(&self, w: &mut Writer) {
        w.u32(self.channel_id);
        w.u256(&self.maximum_target);
    }
    fn decode_payload(r: &mut Reader<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            channel_id: r.u32("channel_id")?,
            maximum_target: r.u256("maximum_target")?,
        })
    }
}

/// §5.3.9 — channel close (channel-scoped).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseChannel {
    pub channel_id: u32,
    pub reason_code: String,
}

impl Message for CloseChannel {
    const EXTENSION: u16 = EXTENSION_TYPE_CORE;
    const MSG_TYPE: u8 = MSG_CLOSE_CHANNEL;
    const CHANNEL_MSG: bool = true;
    fn encode_payload(&self, w: &mut Writer) {
        w.u32(self.channel_id);
        w.str0_255(&self.reason_code);
    }
    fn decode_payload(r: &mut Reader<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            channel_id: r.u32("channel_id")?,
            reason_code: r.str0_255("reason_code")?,
        })
    }
}

/// §5.3.4 — channel open refused (request-scoped, not channel-scoped).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenMiningChannelError {
    pub request_id: u32,
    pub error_code: String,
}

impl Message for OpenMiningChannelError {
    const EXTENSION: u16 = EXTENSION_TYPE_CORE;
    const MSG_TYPE: u8 = MSG_OPEN_MINING_CHANNEL_ERROR;
    const CHANNEL_MSG: bool = false;
    fn encode_payload(&self, w: &mut Writer) {
        w.u32(self.request_id);
        w.str0_255(&self.error_code);
    }
    fn decode_payload(r: &mut Reader<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            request_id: r.u32("request_id")?,
            error_code: r.str0_255("error_code")?,
        })
    }
}

// ---- Lucky Lode extension messages (EXTENSION_LUCKY) ----
//
// The core `NewMiningJob` (merkle root + version + ntime) assumes a
// Bitcoin header the miner reconstructs. LuckyHash grinds an opaque
// template header (doc 05 Addendum A, 109 bytes) with a u64 nonce domain
// and a height-bound salt, so the pool ships the whole template and the
// miner submits the u64 nonce. Acks reuse the core
// `SubmitShares.Success/Error` types unchanged.

/// Lucky job: full header template for one channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LuckyJob {
    pub channel_id: u32,
    pub job_id: u32,
    /// Chain height the template is for (the LuckyHash salt binds it).
    pub height: u64,
    /// Opaque header template bytes (the grind domain appends the nonce).
    pub header: Vec<u8>,
    pub pool_target: [u8; 32],
    pub net_target: [u8; 32],
    /// True when the height changed (miner resets its nonce cursor).
    pub clean: bool,
}

impl Message for LuckyJob {
    const EXTENSION: u16 = EXTENSION_LUCKY;
    const MSG_TYPE: u8 = MSG_LUCKY_JOB;
    const CHANNEL_MSG: bool = true;
    fn encode_payload(&self, w: &mut Writer) {
        w.u32(self.channel_id);
        w.u32(self.job_id);
        w.u64(self.height);
        w.bytes_u16(&self.header);
        w.u256(&self.pool_target);
        w.u256(&self.net_target);
        w.u8(u8::from(self.clean));
    }
    fn decode_payload(r: &mut Reader<'_>) -> Result<Self, CodecError> {
        let channel_id = r.u32("channel_id")?;
        let job_id = r.u32("job_id")?;
        let height = r.u64("height")?;
        let header = r.bytes_u16("header")?;
        if header.is_empty() {
            return Err(CodecError::InvalidValue("header"));
        }
        let pool_target = r.u256("pool_target")?;
        let net_target = r.u256("net_target")?;
        let clean = match r.u8("clean")? {
            0 => false,
            1 => true,
            _ => return Err(CodecError::InvalidValue("clean")),
        };
        Ok(Self {
            channel_id,
            job_id,
            height,
            header,
            pool_target,
            net_target,
            clean,
        })
    }
}

/// Lucky share submit: the u64 grind nonce for a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LuckySubmit {
    pub channel_id: u32,
    pub job_id: u32,
    pub sequence_number: u32,
    pub nonce: u64,
}

impl Message for LuckySubmit {
    const EXTENSION: u16 = EXTENSION_LUCKY;
    const MSG_TYPE: u8 = MSG_LUCKY_SUBMIT;
    const CHANNEL_MSG: bool = true;
    fn encode_payload(&self, w: &mut Writer) {
        w.u32(self.channel_id);
        w.u32(self.job_id);
        w.u32(self.sequence_number);
        w.u64(self.nonce);
    }
    fn decode_payload(r: &mut Reader<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            channel_id: r.u32("channel_id")?,
            job_id: r.u32("job_id")?,
            sequence_number: r.u32("sequence_number")?,
            nonce: r.u64("nonce")?,
        })
    }
}

// ---- public typed encode/decode surface ----

macro_rules! typed_api {
    ($($t:ty),+ $(,)?) => {
        $(
            impl $t {
                /// Encode as a core-spec frame.
                #[must_use]
                pub fn to_frame(&self) -> Frame {
                    to_frame(self)
                }
                /// Decode from a frame (extension/type checked, no
                /// trailing bytes).
                pub fn from_frame(f: &Frame) -> Result<Self, CodecError> {
                    from_frame(f)
                }
            }
        )+
    };
}

typed_api!(
    SetupConnection,
    SetupConnectionSuccess,
    SetupConnectionError,
    OpenStandardMiningChannel,
    OpenStandardMiningChannelSuccess,
    OpenMiningChannelError,
    SubmitSharesStandard,
    SubmitSharesSuccess,
    SubmitSharesError,
    NewMiningJob,
    SetNewPrevHash,
    SetTarget,
    CloseChannel,
    LuckyJob,
    LuckySubmit,
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_header_kat() {
        // extension 0x0000, type 0x1a, payload [01 02 03] — pinned bytes.
        let f = Frame::new(
            EXTENSION_TYPE_CORE,
            MSG_SUBMIT_SHARES_STANDARD,
            false,
            vec![1, 2, 3],
        );
        assert_eq!(
            f.encode(),
            vec![0x00, 0x00, 0x1a, 0x03, 0x00, 0x00, 0x01, 0x02, 0x03]
        );
        let (back, used) = Frame::decode(&f.encode()).expect("decode").expect("frame");
        assert_eq!(used, 9);
        assert_eq!(back, f);
    }

    #[test]
    fn frame_channel_bit_roundtrip() {
        let f = Frame::new(EXTENSION_TYPE_CORE, MSG_SET_TARGET, true, vec![1]);
        assert!(f.channel_msg());
        assert_eq!(f.extension(), EXTENSION_TYPE_CORE);
        assert_eq!(f.extension_type, 0x8000);
        let (back, _) = Frame::decode(&f.encode()).expect("decode").expect("frame");
        assert!(back.channel_msg());
    }

    #[test]
    fn frame_decode_needs_full_payload() {
        let f = Frame::new(EXTENSION_TYPE_CORE, MSG_RECONNECT, false, vec![0; 100]);
        let bytes = f.encode();
        for cut in [0usize, 1, 5, 6, 50, 105] {
            assert_eq!(Frame::decode(&bytes[..cut]).expect("partial ok"), None);
        }
        assert!(Frame::decode(&bytes).expect("full ok").is_some());
    }

    #[test]
    fn frame_reader_handles_split_and_batched_feeds() {
        let a = Frame::new(
            EXTENSION_TYPE_CORE,
            MSG_SETUP_CONNECTION,
            false,
            vec![0xaa; 40],
        );
        let b = Frame::new(EXTENSION_TYPE_CORE, MSG_SET_TARGET, true, vec![0xbb; 33]);
        let mut wire = a.encode();
        wire.extend_from_slice(&b.encode());

        // Split at every boundary that matters (header, mid-payload).
        let mut r = FrameReader::new();
        for chunk in [
            &wire[..1],
            &wire[1..4],
            &wire[4..9],
            &wire[9..50],
            &wire[50..],
        ] {
            r.feed(chunk);
        }
        assert_eq!(r.next_frame().expect("frame a"), Some(a));
        assert_eq!(r.next_frame().expect("frame b"), Some(b));
        assert_eq!(r.next_frame().expect("empty"), None);
        assert_eq!(r.buffered(), 0);

        // Two frames in one feed.
        let mut r = FrameReader::new();
        r.feed(&wire);
        assert!(r.next_frame().expect("a").is_some());
        assert!(r.next_frame().expect("b").is_some());
        assert_eq!(r.next_frame().expect("none"), None);
    }

    #[test]
    fn frame_reader_rejects_oversize() {
        let mut bytes = vec![0x00, 0x00, 0x1a];
        let n = (MAX_FRAME_LEN + 1) as u32;
        bytes.extend_from_slice(&[n as u8, (n >> 8) as u8, (n >> 16) as u8]);
        assert_eq!(
            Frame::decode(&bytes),
            Err(FrameError::PayloadTooLarge(MAX_FRAME_LEN + 1))
        );
    }

    #[test]
    fn setup_connection_roundtrip_and_layout() {
        let m = SetupConnection {
            protocol: PROTOCOL_MINING,
            min_version: 2,
            max_version: 2,
            flags: 0,
            endpoint_host: "luckylode.cc".to_string(),
            endpoint_port: 3346,
            vendor: "LuckyLode".to_string(),
            hardware_version: "lm-miner/0.2".to_string(),
            firmware: "0.2".to_string(),
            device_id: String::new(),
        };
        let f = m.to_frame();
        assert_eq!(f.msg_type, MSG_SETUP_CONNECTION);
        assert!(!f.channel_msg());
        // Field layout prefix: protocol(1) min(2) max(2) flags(4).
        assert_eq!(&f.payload[..9], &[0x00, 0x02, 0x00, 0x02, 0x00, 0, 0, 0, 0]);
        assert_eq!(SetupConnection::from_frame(&f).expect("decode"), m);
    }

    #[test]
    fn submit_shares_standard_payload_kat() {
        let m = SubmitSharesStandard {
            channel_id: 1,
            sequence_number: 2,
            job_id: 3,
            nonce: 0xdead_beef,
            ntime: 0x1122_3344,
            version: 0x2000_0000,
        };
        let f = m.to_frame();
        assert!(f.channel_msg());
        assert_eq!(f.extension_type, CHANNEL_MSG_BIT); // core + channel bit
        let mut expect = Vec::new();
        expect.extend_from_slice(&1u32.to_le_bytes());
        expect.extend_from_slice(&2u32.to_le_bytes());
        expect.extend_from_slice(&3u32.to_le_bytes());
        expect.extend_from_slice(&0xdead_beefu32.to_le_bytes());
        expect.extend_from_slice(&0x1122_3344u32.to_le_bytes());
        expect.extend_from_slice(&0x2000_0000u32.to_le_bytes());
        assert_eq!(f.payload, expect);
        assert_eq!(SubmitSharesStandard::from_frame(&f).expect("decode"), m);
    }

    #[test]
    fn new_mining_job_optional_ntime() {
        let base = NewMiningJob {
            channel_id: 7,
            job_id: 9,
            ntime_start: Some(1_700_000_000),
            version: 4,
            merkle_root: [0x42; 32],
        };
        assert_eq!(
            NewMiningJob::from_frame(&base.to_frame()).expect("active"),
            base
        );
        let future = NewMiningJob {
            ntime_start: None,
            ..base
        };
        let f = future.to_frame();
        // option flag byte sits after channel_id + job_id.
        assert_eq!(f.payload[8], 0x00);
        assert_eq!(NewMiningJob::from_frame(&f).expect("future"), future);
        // Bad option discriminant is rejected.
        let mut bad = f.clone();
        bad.payload[8] = 2;
        assert_eq!(
            NewMiningJob::from_frame(&bad),
            Err(CodecError::InvalidValue("ntime_start"))
        );
    }

    #[test]
    fn set_new_prev_hash_and_set_target_roundtrip() {
        let p = SetNewPrevHash {
            channel_id: 3,
            job_id: 4,
            prev_hash: [0x11; 32],
            ntime: 1_700_000_100,
        };
        assert_eq!(SetNewPrevHash::from_frame(&p.to_frame()).expect("prev"), p);
        let t = SetTarget {
            channel_id: 3,
            maximum_target: [0x0f; 32],
        };
        assert_eq!(SetTarget::from_frame(&t.to_frame()).expect("target"), t);
    }

    #[test]
    fn submit_success_and_error_roundtrip() {
        let s = SubmitSharesSuccess {
            channel_id: 1,
            last_sequence_number: 10,
            new_submits_accepted_count: 10,
            new_shares_sum: 10_000_000_000,
        };
        assert_eq!(
            SubmitSharesSuccess::from_frame(&s.to_frame()).expect("ok"),
            s
        );
        let e = SubmitSharesError {
            channel_id: 1,
            sequence_number: 11,
            error_code: "difficulty-too-low".to_string(),
        };
        assert_eq!(
            SubmitSharesError::from_frame(&e.to_frame()).expect("err"),
            e
        );
    }

    #[test]
    fn codec_rejects_bad_frames() {
        // Wrong extension (an extension message we don't speak).
        let mut f = SetupConnectionSuccess {
            used_version: 2,
            flags: 0,
        }
        .to_frame();
        f.extension_type = 0x4000;
        assert_eq!(
            SetupConnectionSuccess::from_frame(&f),
            Err(CodecError::WrongExtension(0x4000))
        );
        // Wrong type for the target struct.
        let f = SetupConnectionSuccess {
            used_version: 2,
            flags: 0,
        }
        .to_frame();
        assert_eq!(
            SetupConnectionError::from_frame(&f),
            Err(CodecError::WrongType(MSG_SETUP_CONNECTION_SUCCESS))
        );
        // Trailing bytes (unnegotiated TLV) are refused for now.
        let mut f = SetupConnectionSuccess {
            used_version: 2,
            flags: 0,
        }
        .to_frame();
        f.payload.push(0xff);
        assert_eq!(
            SetupConnectionSuccess::from_frame(&f),
            Err(CodecError::TrailingBytes(1))
        );
        // Truncated payload.
        let f = SetupConnectionSuccess {
            used_version: 2,
            flags: 0,
        }
        .to_frame();
        let short = Frame {
            payload: f.payload[..3].to_vec(),
            ..f
        };
        assert_eq!(
            SetupConnectionSuccess::from_frame(&short),
            Err(CodecError::UnexpectedEof("flags"))
        );
    }

    #[test]
    fn hash_rate_validation() {
        let m = OpenStandardMiningChannel {
            request_id: 1,
            user_identity: "worker".to_string(),
            nominal_hash_rate: 14_500.0,
            max_target: [0xff; 32],
        };
        assert_eq!(
            OpenStandardMiningChannel::from_frame(&m.to_frame()).expect("ok"),
            m
        );
        for bad in [f32::NAN, f32::INFINITY, -1.0] {
            let mut f = m.to_frame();
            // rate sits right before the 32-byte target.
            let at = f.payload.len() - 32 - 4;
            f.payload[at..at + 4].copy_from_slice(&bad.to_le_bytes());
            assert_eq!(
                OpenStandardMiningChannel::from_frame(&f),
                Err(CodecError::InvalidValue("nominal_hash_rate"))
            );
        }
    }

    #[test]
    fn lucky_job_and_submit_roundtrip_with_kat() {
        let job = LuckyJob {
            channel_id: 1,
            job_id: 9,
            height: 1234,
            header: vec![0xabu8; 109],
            pool_target: [0x01; 32],
            net_target: [0x00; 32],
            clean: true,
        };
        let f = job.to_frame();
        assert_eq!(f.extension_type, EXTENSION_LUCKY | CHANNEL_MSG_BIT);
        assert_eq!(f.msg_type, MSG_LUCKY_JOB);
        // Layout: channel(4) job(4) height(8) header_len(2)+109 pool(32)
        // net(32) clean(1).
        assert_eq!(f.payload.len(), 4 + 4 + 8 + 2 + 109 + 32 + 32 + 1);
        assert_eq!(&f.payload[..4], &1u32.to_le_bytes());
        assert_eq!(&f.payload[4..8], &9u32.to_le_bytes());
        assert_eq!(&f.payload[8..16], &1234u64.to_le_bytes());
        assert_eq!(&f.payload[16..18], &109u16.to_le_bytes());
        assert_eq!(f.payload[16 + 2 + 109 + 64], 0x01);
        assert_eq!(LuckyJob::from_frame(&f).expect("decode"), job);
        // Empty header is refused (nothing to grind).
        let mut bad = f.clone();
        bad.payload[16..18].copy_from_slice(&0u16.to_le_bytes());
        bad.payload.drain(18..18 + 109);
        // Fix the length in a rebuilt frame (payload shrank by 109).
        let short = Frame::new(EXTENSION_LUCKY, MSG_LUCKY_JOB, true, bad.payload);
        assert_eq!(
            LuckyJob::from_frame(&short),
            Err(CodecError::InvalidValue("header"))
        );
        // Bad clean discriminant is refused.
        let mut bad_clean = job.to_frame();
        let at = bad_clean.payload.len() - 1;
        bad_clean.payload[at] = 2;
        assert_eq!(
            LuckyJob::from_frame(&bad_clean),
            Err(CodecError::InvalidValue("clean"))
        );
        // Core decoders reject the Lucky extension and vice versa.
        assert_eq!(
            SetupConnectionSuccess::from_frame(&job.to_frame()),
            Err(CodecError::WrongExtension(EXTENSION_LUCKY))
        );

        let sub = LuckySubmit {
            channel_id: 1,
            job_id: 9,
            sequence_number: 3,
            nonce: 0xdead_beef_1234_5678,
        };
        let sf = sub.to_frame();
        assert_eq!(sf.extension_type, EXTENSION_LUCKY | CHANNEL_MSG_BIT);
        assert_eq!(sf.msg_type, MSG_LUCKY_SUBMIT);
        assert_eq!(sf.payload.len(), 4 + 4 + 4 + 8);
        assert_eq!(&sf.payload[..4], &1u32.to_le_bytes());
        assert_eq!(&sf.payload[4..8], &9u32.to_le_bytes());
        assert_eq!(&sf.payload[8..12], &3u32.to_le_bytes());
        assert_eq!(&sf.payload[12..20], &0xdead_beef_1234_5678u64.to_le_bytes());
        assert_eq!(LuckySubmit::from_frame(&sf).expect("decode"), sub);
    }

    #[test]
    fn open_channel_error_roundtrip() {
        let e = OpenMiningChannelError {
            request_id: 42,
            error_code: "invalid credentials".to_string(),
        };
        let f = e.to_frame();
        assert_eq!(f.msg_type, MSG_OPEN_MINING_CHANNEL_ERROR);
        assert!(!f.channel_msg());
        assert_eq!(OpenMiningChannelError::from_frame(&f).expect("decode"), e);
    }

    #[test]
    fn extranonce_prefix_length_guard() {
        let m = OpenStandardMiningChannelSuccess {
            request_id: 1,
            channel_id: 2,
            target: [0; 32],
            extranonce_prefix: vec![0; 32],
            group_channel_id: 0,
        };
        assert_eq!(
            OpenStandardMiningChannelSuccess::from_frame(&m.to_frame()).expect("ok"),
            m
        );
        let mut f = m.to_frame();
        // Length byte sits after request_id + channel_id + target.
        f.payload[8 + 32] = 33;
        assert_eq!(
            OpenStandardMiningChannelSuccess::from_frame(&f),
            Err(CodecError::LengthOutOfRange("extranonce_prefix", 33))
        );
    }
}
