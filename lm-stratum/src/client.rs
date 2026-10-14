//! Blocking pool client: subscribe → authorize → jobs → submit.
//!
//! Written for tooling and the e2e suite (not for hash-rate — grinding
//! belongs in `lm-miner`). Strictly sequential: one outstanding expectation
//! at a time, responses matched by arrival order.

use crate::tls::{self, ReadWrite};
use crate::v1::{job_from_params, Job, Message};
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub struct PoolClient {
    /// Single read/write stream (plain or TLS); writes go through
    /// `get_mut()` so a TLS session's connection state stays in one place.
    stream: BufReader<Box<dyn ReadWrite>>,
    /// Raw socket clone, kept for read-timeout control (`poll_job`).
    sock: TcpStream,
    next_id: u64,
    /// Notifies that arrived while waiting for a response (id-matched reads
    /// must not swallow them — Stratum multiplexes jobs over responses).
    pending: Vec<Job>,
    /// Per-RPC response deadline. 10 s is right for a healthy pool; slow
    /// or heavily loaded boxes (a reference-pool testbed that also mines)
    /// need more, hence [`PoolClient::connect_patient`].
    rpc_deadline: Duration,
    /// Accumulated partial input. A line that spans two socket reads (TCP
    /// segmentation, Nagle) must survive the read timeout between them —
    /// `read_line` into a fresh string per call silently ate half-received
    /// messages (live-caught: lost submit responses -> reconnect loops).
    rx_buf: String,
}

impl PoolClient {
    pub fn connect(addr: SocketAddr) -> std::io::Result<Self> {
        Self::connect_patient(addr, Duration::from_secs(10))
    }

    /// Connect with a custom per-RPC deadline.
    pub fn connect_patient(addr: SocketAddr, rpc_deadline: Duration) -> std::io::Result<Self> {
        Self::connect_inner(addr, None, rpc_deadline)
    }

    /// Connect over TLS with a custom deadline. `server_name` must match
    /// the certificate (the pool hostname). `ca` adds a private CA;
    /// `insecure` skips verification (tests only).
    pub fn connect_tls(
        addr: SocketAddr,
        server_name: &str,
        ca: Option<&Path>,
        insecure: bool,
        rpc_deadline: Duration,
    ) -> std::io::Result<Self> {
        let cfg = tls::client_config(ca, insecure)?;
        Self::connect_inner(addr, Some((server_name, cfg)), rpc_deadline)
    }

    fn connect_inner(
        addr: SocketAddr,
        tls: Option<(&str, Arc<rustls::ClientConfig>)>,
        rpc_deadline: Duration,
    ) -> std::io::Result<Self> {
        let sock = TcpStream::connect_timeout(&addr, Duration::from_secs(5))?;
        sock.set_read_timeout(Some(Duration::from_secs(5)))?;
        sock.set_write_timeout(Some(Duration::from_secs(5)))?;
        // Kill Nagle: `writeln!` issues body and newline as two writes, and
        // Nagle + delayed ACK stretch the gap to ~100 ms — right at the
        // pool's read tick, which is how half-submits got lost.
        let _ = sock.set_nodelay(true);
        let io = match tls {
            Some((name, cfg)) => tls::connect(sock.try_clone()?, name, Some(&cfg))?,
            None => tls::connect(sock.try_clone()?, "", None)?,
        };
        Ok(Self {
            stream: BufReader::new(io),
            sock,
            next_id: 1,
            pending: Vec::new(),
            rpc_deadline,
            rx_buf: String::new(),
        })
    }

    fn rpc(&mut self, method: &str, params: serde_json::Value) -> std::io::Result<Message> {
        let id = self.next_id;
        self.next_id += 1;
        let line = serde_json::json!({"id": id, "method": method, "params": params}).to_string();
        writeln!(self.stream.get_mut(), "{line}")?;
        let want = serde_json::Value::from(id);
        let deadline = Instant::now() + self.rpc_deadline;
        while Instant::now() < deadline {
            match self.read_message()? {
                Some(m) if m.id == Some(want.clone()) => return Ok(m),
                Some(m) if m.method.as_deref() == Some("mining.notify") => {
                    if let Some(p) = m.params.as_ref() {
                        if let Some(job) = job_from_params(p) {
                            self.pending.push(job);
                        }
                    }
                }
                Some(_) => continue, // another response; shouldn't happen sequentially
                None => continue,    // timeout tick / blank line
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "no response from pool",
        ))
    }

    fn read_message(&mut self) -> std::io::Result<Option<Message>> {
        loop {
            if let Some(pos) = self.rx_buf.find('\n') {
                let line: String = self.rx_buf.drain(..=pos).collect();
                if line.trim().is_empty() {
                    continue;
                }
                return serde_json::from_str(line.trim())
                    .map(Some)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e));
            }
            let mut chunk = String::new();
            match self.stream.read_line(&mut chunk) {
                // Clean EOF: the pool closed the connection. This MUST be an
                // error, not "no data yet" — treating it as a timeout made
                // read_job/poll_job spin (or block) on a dead socket until
                // the job deadline instead of reconnecting (live-caught:
                // miner sat in CLOSE-WAIT for minutes after a pool restart).
                Ok(0) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "pool closed the connection",
                    ))
                }
                Ok(_) => self.rx_buf.push_str(&chunk),
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    return Ok(None)
                }
                Err(e) => return Err(e),
            }
        }
    }

    pub fn subscribe(&mut self, agent: &str) -> std::io::Result<bool> {
        let m = self.rpc("mining.subscribe", serde_json::json!([agent]))?;
        Ok(m.error.is_none() || m.error == Some(serde_json::Value::Null))
    }

    pub fn authorize(&mut self, worker: &str) -> std::io::Result<bool> {
        self.authorize_with(worker, "x")
    }

    /// Authorize with an explicit pool password (see `--pool-password` on
    /// the pool). Empty password is sent for open pools.
    pub fn authorize_with(&mut self, worker: &str, password: &str) -> std::io::Result<bool> {
        let m = self.rpc(
            "mining.authorize",
            serde_json::json!([worker, password]),
        )?;
        Ok(m.result == Some(serde_json::Value::Bool(true)))
    }

    /// Next `mining.notify` job: buffered arrivals first, then the socket.
    /// Waits up to `timeout`.
    pub fn read_job(&mut self, timeout: Duration) -> std::io::Result<Option<Job>> {
        if let Some(job) = self.pending.pop() {
            return Ok(Some(job));
        }
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            match self.read_message()? {
                Some(m) if m.method.as_deref() == Some("mining.notify") => {
                    if let Some(p) = m.params.as_ref() {
                        if let Some(job) = job_from_params(p) {
                            return Ok(Some(job));
                        }
                    }
                }
                Some(_) => continue, // a response arriving late; ignore here
                None => continue,    // timeout tick or blank line
            }
        }
        Ok(None)
    }

    /// Non-blocking job check for a grinding miner: drain buffered
    /// notifies, else peek the socket with a 1 ms read timeout (std
    /// rejects a zero timeout). A missing job just means "keep the
    /// current one".
    pub fn poll_job(&mut self) -> std::io::Result<Option<Job>> {
        if let Some(job) = self.pending.pop() {
            return Ok(Some(job));
        }
        self.sock.set_read_timeout(Some(Duration::from_millis(1)))?;
        let r = self.read_message();
        self.sock.set_read_timeout(Some(Duration::from_secs(5)))?;
        match r? {
            Some(m) if m.method.as_deref() == Some("mining.notify") => {
                Ok(m.params.as_ref().and_then(job_from_params))
            }
            _ => Ok(None),
        }
    }

    /// Submit a nonce; returns (accepted, error tuple if rejected).
    pub fn submit(
        &mut self,
        worker: &str,
        job_id: &str,
        nonce: u64,
    ) -> std::io::Result<(bool, Option<(i64, String)>)> {
        let m = self.rpc(
            "mining.submit",
            serde_json::json!([worker, job_id, format!("{nonce:016x}")]),
        )?;
        if m.result == Some(serde_json::Value::Bool(true)) {
            return Ok((true, None));
        }
        let err = m.error.and_then(|e| {
            let a = e.as_array()?;
            Some((a.first()?.as_i64()?, a.get(1)?.as_str()?.to_string()))
        });
        Ok((false, err))
    }

    /// Send a raw line and read one message back (protocol-edge tests).
    pub fn raw(&mut self, line: &str) -> std::io::Result<Message> {
        writeln!(self.stream.get_mut(), "{line}")?;
        self.read_message()?.ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::TimedOut, "no response from pool")
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn eof_on_dead_connection_is_an_error_not_a_wait() {
        // Regression (live 2026-10-12): a pool restart left the miner in
        // CLOSE-WAIT until the job deadline because EOF mapped to
        // Ok(None) ("no data yet"). It must surface immediately so the
        // miner's reconnect path runs.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let handle = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            drop(stream); // FIN with no bytes
        });
        let mut client = PoolClient::connect(addr).expect("connect");
        let t0 = Instant::now();
        let r = client.read_job(Duration::from_secs(5));
        assert_eq!(
            r.expect_err("EOF must be an error").kind(),
            std::io::ErrorKind::UnexpectedEof
        );
        assert!(
            t0.elapsed() < Duration::from_secs(1),
            "must not wait for the deadline, waited {:?}",
            t0.elapsed()
        );
        // poll_job must surface it too (the grinding loop's fast path).
        assert!(client.poll_job().is_err());
        handle.join().expect("join");
    }
}
