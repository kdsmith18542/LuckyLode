//! Blocking pool client: subscribe → authorize → jobs → submit.
//!
//! Written for tooling and the e2e suite (not for hash-rate — grinding
//! belongs in `lm-miner`). Strictly sequential: one outstanding expectation
//! at a time, responses matched by arrival order.

use crate::v1::{job_from_params, Job, Message};
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

pub struct PoolClient {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
    next_id: u64,
    /// Notifies that arrived while waiting for a response (id-matched reads
    /// must not swallow them — Stratum multiplexes jobs over responses).
    pending: Vec<Job>,
    /// Per-RPC response deadline. 10 s is right for a healthy pool; slow
    /// or heavily loaded boxes (a reference-pool testbed that also mines)
    /// need more, hence [`PoolClient::connect_patient`].
    rpc_deadline: Duration,
}

impl PoolClient {
    pub fn connect(addr: SocketAddr) -> std::io::Result<Self> {
        Self::connect_patient(addr, Duration::from_secs(10))
    }

    /// Connect with a custom per-RPC deadline.
    pub fn connect_patient(addr: SocketAddr, rpc_deadline: Duration) -> std::io::Result<Self> {
        let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5))?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        let reader = BufReader::new(stream.try_clone()?);
        Ok(Self {
            reader,
            writer: stream,
            next_id: 1,
            pending: Vec::new(),
            rpc_deadline,
        })
    }

    fn rpc(&mut self, method: &str, params: serde_json::Value) -> std::io::Result<Message> {
        let id = self.next_id;
        self.next_id += 1;
        let line = serde_json::json!({"id": id, "method": method, "params": params}).to_string();
        writeln!(self.writer, "{line}")?;
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
        Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "no response from pool"))
    }

    fn read_message(&mut self) -> std::io::Result<Option<Message>> {
        let mut line = String::new();
        match self.reader.read_line(&mut line) {
            Ok(0) => Ok(None),
            Ok(_) => {
                if line.trim().is_empty() {
                    return Ok(None);
                }
                serde_json::from_str(line.trim())
                    .map(Some)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }

    pub fn subscribe(&mut self, agent: &str) -> std::io::Result<bool> {
        let m = self.rpc("mining.subscribe", serde_json::json!([agent]))?;
        Ok(m.error.is_none() || m.error == Some(serde_json::Value::Null))
    }

    pub fn authorize(&mut self, worker: &str) -> std::io::Result<bool> {
        let m = self.rpc("mining.authorize", serde_json::json!([worker, "x"]))?;
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
        writeln!(self.writer, "{line}")?;
        self.read_message()?.ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::TimedOut, "no response from pool")
        })
    }
}
