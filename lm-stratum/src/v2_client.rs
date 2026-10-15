//! Blocking V2 pool client: Noise handshake → `SetupConnection` → open
//! standard channel → [`LuckyJob`] stream → [`LuckySubmit`] + acks.
//!
//! Mirrors [`crate::client::PoolClient`] semantics (strictly sequential; a
//! pending-job queue so a rotation arriving between submit and ack is not
//! lost), but over an encrypted [`V2Session`] and the Lucky extension
//! messages. One standard channel per connection for this milestone.

use std::io;
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use crate::v2::{
    from_frame, to_frame, LuckyJob, LuckySubmit, OpenMiningChannelError, OpenStandardMiningChannel,
    OpenStandardMiningChannelSuccess, SetupConnection, SubmitSharesError, SubmitSharesSuccess,
    MSG_LUCKY_JOB, MSG_OPEN_MINING_CHANNEL_ERROR, MSG_OPEN_STANDARD_MINING_CHANNEL_SUCCESS,
    MSG_SUBMIT_SHARES_ERROR, MSG_SUBMIT_SHARES_SUCCESS,
};
use crate::v2_session::{SessionError, V2Session, setup_connection};

/// Blocking V2 miner client (one channel).
pub struct V2Client {
    session: V2Session<TcpStream>,
    pending: Vec<LuckyJob>,
    sequence: u32,
}

fn io_timeout(e: &SessionError) -> bool {
    matches!(e, SessionError::Io(io) if {
        io.kind() == io::ErrorKind::WouldBlock || io.kind() == io::ErrorKind::TimedOut
    })
}

impl V2Client {
    /// Connect, handshake and negotiate the connection version.
    ///
    /// # Errors
    /// [`SessionError`] on TCP, handshake, negotiation or rejection failures.
    pub fn connect(
        addr: SocketAddr,
        authority: Option<[u8; 32]>,
        setup: &SetupConnection,
    ) -> Result<Self, SessionError> {
        let sock = TcpStream::connect_timeout(&addr, Duration::from_secs(5))?;
        let _ = sock.set_nodelay(true);
        sock.set_read_timeout(Some(Duration::from_secs(10)))?;
        sock.set_write_timeout(Some(Duration::from_secs(10)))?;
        let mut session = V2Session::connect(sock, authority)?;
        setup_connection(&mut session, setup)?;
        Ok(Self {
            session,
            pending: Vec::new(),
            sequence: 0,
        })
    }

    /// Open one standard channel for `user_identity` (the worker name).
    ///
    /// # Errors
    /// [`SessionError::Rejected`] when the pool refuses (bad credentials);
    /// other [`SessionError`] on transport failures.
    pub fn open_channel(
        &mut self,
        request_id: u32,
        user_identity: &str,
        nominal_hash_rate: f32,
        max_target: [u8; 32],
    ) -> Result<OpenStandardMiningChannelSuccess, SessionError> {
        self.session.send(&to_frame(&OpenStandardMiningChannel {
            request_id,
            user_identity: user_identity.to_string(),
            nominal_hash_rate,
            max_target,
        }))?;
        self.session
            .set_read_timeout(Some(Duration::from_secs(10)))?;
        loop {
            let frame = self.session.recv()?;
            match frame.msg_type {
                MSG_OPEN_STANDARD_MINING_CHANNEL_SUCCESS => {
                    return from_frame::<OpenStandardMiningChannelSuccess>(&frame)
                        .map_err(SessionError::Codec);
                }
                MSG_OPEN_MINING_CHANNEL_ERROR => {
                    let e = from_frame::<OpenMiningChannelError>(&frame)
                        .map_err(SessionError::Codec)?;
                    return Err(SessionError::Rejected(e.error_code));
                }
                MSG_LUCKY_JOB => {
                    // A rotation raced the open: keep it for the job loop.
                    if let Ok(job) = from_frame::<LuckyJob>(&frame) {
                        self.pending.push(job);
                    }
                    continue;
                }
                _ => continue,
            }
        }
    }

    /// Non-blocking job check for a grinding miner: buffered arrivals
    /// first, else a 1 ms socket peek. Anything that is not a job is
    /// ignored here (acks belong to [`V2Client::submit`], which the miner
    /// always drains before grinding on).
    ///
    /// # Errors
    /// [`SessionError`] on transport failures (timeouts become `Ok(None)`).
    pub fn poll_job(&mut self) -> Result<Option<LuckyJob>, SessionError> {
        if let Some(job) = self.pending.pop() {
            return Ok(Some(job));
        }
        self.session
            .set_read_timeout(Some(Duration::from_millis(1)))?;
        match self.session.recv() {
            Ok(frame) => {
                if frame.msg_type != MSG_LUCKY_JOB {
                    return Ok(None);
                }
                from_frame::<LuckyJob>(&frame)
                    .map(Some)
                    .map_err(SessionError::Codec)
            }
            Err(e) if io_timeout(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Next [`LuckyJob`], waiting up to `timeout`.
    ///
    /// # Errors
    /// [`SessionError`] on transport failures.
    pub fn read_job(&mut self, timeout: Duration) -> Result<Option<LuckyJob>, SessionError> {
        if let Some(job) = self.pending.pop() {
            return Ok(Some(job));
        }
        self.session
            .set_read_timeout(Some(Duration::from_millis(50)))?;
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            match self.session.recv() {
                Ok(frame) => {
                    if frame.msg_type != MSG_LUCKY_JOB {
                        continue;
                    }
                    return from_frame::<LuckyJob>(&frame)
                        .map(Some)
                        .map_err(SessionError::Codec);
                }
                Err(e) if io_timeout(&e) => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(None)
    }

    /// Submit a nonce. Returns `(accepted, rejection code)` like
    /// [`crate::client::PoolClient::submit`]: `Ok((true, None))` accepted,
    /// `Ok((false, Some(code)))` rejected by the pool, `Err` on transport
    /// failure. A job rotation arriving first is queued in `pending`,
    /// never lost.
    ///
    /// # Errors
    /// Transport failures as strings.
    pub fn submit(
        &mut self,
        channel_id: u32,
        job_id: u32,
        nonce: u64,
    ) -> Result<(bool, Option<String>), String> {
        let sequence = self.sequence;
        self.sequence = self.sequence.wrapping_add(1);
        self.session
            .send(&to_frame(&LuckySubmit {
                channel_id,
                job_id,
                sequence_number: sequence,
                nonce,
            }))
            .map_err(|e| e.to_string())?;
        self.session
            .set_read_timeout(Some(Duration::from_secs(10)))
            .map_err(|e| e.to_string())?;
        loop {
            let frame = self.session.recv().map_err(|e| e.to_string())?;
            match frame.msg_type {
                MSG_LUCKY_JOB => {
                    if let Ok(job) = LuckyJob::from_frame(&frame) {
                        self.pending.push(job);
                    }
                    continue;
                }
                MSG_SUBMIT_SHARES_SUCCESS => {
                    let ack = SubmitSharesSuccess::from_frame(&frame)
                        .map_err(|e| format!("{e:?}"))?;
                    if ack.last_sequence_number != sequence {
                        continue; // not ours; sequential sessions never hit this
                    }
                    return Ok((true, None));
                }
                MSG_SUBMIT_SHARES_ERROR => {
                    let nack = SubmitSharesError::from_frame(&frame)
                        .map_err(|e| format!("{e:?}"))?;
                    if nack.sequence_number != sequence {
                        continue;
                    }
                    return Ok((false, Some(nack.error_code)));
                }
                _ => continue,
            }
        }
    }

    /// Next sequence number that [`V2Client::submit`] will use (logs).
    #[must_use]
    pub fn next_sequence(&self) -> u32 {
        self.sequence
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::MockProvider;
    use crate::server::spawn_server;
    use crate::v2::PROTOCOL_MINING;
    use std::collections::HashMap;

    fn setup() -> SetupConnection {
        SetupConnection {
            protocol: PROTOCOL_MINING,
            min_version: 2,
            max_version: 2,
            flags: 0,
            endpoint_host: "127.0.0.1".to_string(),
            endpoint_port: 3346,
            vendor: "lm-miner".to_string(),
            hardware_version: "cpu".to_string(),
            firmware: "0.2".to_string(),
            device_id: "x".to_string(),
        }
    }

    /// Full client round trip against a live V2 listener on the mock chain:
    /// connect → open → job → submit → accepted, with the mock's instant
    /// blocks crediting the miner.
    #[test]
    fn v2_client_round_trip_on_mock_chain() {
        let authority = crate::v2_session::generate_authority();
        let mut server =
            spawn_server(MockProvider::instant(1_000_000), "127.0.0.1:0", None, None, None)
                .expect("spawn");
        let v2_addr = server
            .spawn_v2_listener("127.0.0.1:0", authority, Duration::from_secs(3600))
            .expect("v2 listen");
        let mut client = V2Client::connect(v2_addr, Some(authority.0), &setup()).expect("connect");
        let opened = client
            .open_channel(7, "worker-v2", 1_000.0, [0xffu8; 32])
            .expect("open");
        assert_ne!(opened.channel_id, 0);
        let job = client
            .read_job(Duration::from_secs(5))
            .expect("read job")
            .expect("job");
        assert_eq!(job.channel_id, opened.channel_id);
        assert!(!job.header.is_empty());
        // Instant mock chain: nonce 0 is both a share and a block.
        let (accepted, err) = client
            .submit(opened.channel_id, job.job_id, 0)
            .expect("submit");
        assert!(accepted, "rejected: {err:?}");
        // The same share twice is a duplicate.
        let (accepted, err) = client.submit(opened.channel_id, job.job_id, 0).expect("resubmit");
        assert!(!accepted);
        assert_eq!(err.as_deref(), Some("duplicate share"));
        // Money moved through the shared ledger.
        assert_eq!(server.blocks().len(), 1);
        let miner = crate::v1::worker_id("worker-v2");
        assert!(server.paid(miner) + server.balances().iter().map(|(_, o)| o).sum::<u64>() > 0);
        server.stop();
    }

    /// Wrong password (device_id) is refused with the pool's credentials
    /// error, mirroring the V1 authorize path.
    #[test]
    fn v2_client_auth_failure() {
        let authority = crate::v2_session::generate_authority();
        let mut tokens = HashMap::new();
        tokens.insert("gated".to_string(), "tok-g".to_string());
        let mut server = spawn_server(
            MockProvider::instant(1_000_000),
            "127.0.0.1:0",
            None,
            Some(tokens),
            None,
        )
        .expect("spawn");
        let v2_addr = server
            .spawn_v2_listener("127.0.0.1:0", authority, Duration::from_secs(3600))
            .expect("v2 listen");
        let mut bad = setup();
        bad.device_id = "wrong".to_string();
        let mut client =
            V2Client::connect(v2_addr, Some(authority.0), &bad).expect("connect");
        let err = client
            .open_channel(1, "gated", 1_000.0, [0xffu8; 32])
            .unwrap_err();
        assert!(
            matches!(err, SessionError::Rejected(_)),
            "expected rejection, got {err:?}"
        );
        server.stop();
    }
}
