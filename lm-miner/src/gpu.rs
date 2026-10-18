//! GPU (OpenCL) LuckyHash backend: doc 15's split as a drop-in search.
//!
//! CPU does both BLAKE3s, the Argon2 prep (H0 + initial blocks), and the
//! H' finalize; the GPU runs only the memory-hard fill (doc 15 §15.1).
//! Results are byte-identical to [`lm_pow::pow_full`] — the KAT gate
//! holds the kernel to that — and the 1080 is 5-10x this box's CPU
//! (doc 15 §15.7).
//!
//! Thermal/power (doc 15 §15.5): the GPU search loop reads the CARD's
//! temperature — AMD via hwmon, NVIDIA via `nvidia-smi` (cached 2 s) —
//! and `--max-temp`/`--duty` pace it through the same policy as the CPU
//! path, with a proportional band so it settles below the cap instead of
//! pause/resume sawtooth. `--max-power W` applies `nvidia-smi -pl` when
//! the driver permits.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use lm_pow::{final_digest, meets_target, seed as pow_seed};

use crate::argon2_prep;
use crate::opencl::GpuDevice;
use crate::thermal::MiningPolicy;
use crate::{BenchReport, Solution};

/// Default Argon2 instances per launch: 512 MiB of VRAM (1 MiB each).
pub const DEFAULT_BATCH: usize = 512;

/// Cached (timestamp, temp C, power W). The nvidia-smi query costs
/// 30-60 ms; a 2 s TTL keeps the per-chunk thermal loop free while
/// staying fresh enough for pacing.
static GPU_STATUS: Mutex<Option<(Instant, i32, Option<u32>)>> = Mutex::new(None);

/// AMD GPU temp/power from hwmon (amdgpu): temp1_input (milli-C),
/// power1_average (micro-W).
fn amdgpu_hwmon() -> Option<(i32, Option<u32>)> {
    for hwmon in 0..16 {
        let dir = format!("/sys/class/hwmon/hwmon{hwmon}");
        let name = match std::fs::read_to_string(format!("{dir}/name")) {
            Ok(n) => n,
            Err(_) => continue,
        };
        if !name.starts_with("amdgpu") {
            continue;
        }
        let temp = std::fs::read_to_string(format!("{dir}/temp1_input"))
            .ok()
            .and_then(|s| s.trim().parse::<i64>().ok())
            .map(|m| (m / 1000) as i32)?;
        let power = std::fs::read_to_string(format!("{dir}/power1_average"))
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .map(|uw| (uw / 1_000_000) as u32);
        return Some((temp, power));
    }
    None
}

/// NVIDIA temp/power via nvidia-smi (no NVML dependency; the query is
/// cached by [`gpu_status`]).
fn nvidia_status() -> Option<(i32, Option<u32>)> {
    let out = std::process::Command::new("nvidia-smi")
        .args([
            "--query-gpu=temperature.gpu,power.draw",
            "--format=csv,noheader,nounits",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    let mut it = s.lines().next()?.split(',').map(|p| p.trim());
    let temp: i32 = it.next()?.parse().ok()?;
    let power = it
        .next()
        .and_then(|p| p.parse::<f64>().ok())
        .map(|w| w.round() as u32);
    Some((temp, power))
}

/// GPU `(temp C, power W)` — `-1`/`None` when no sensor is available.
/// AMD hwmon first (free), then nvidia-smi; cached for 2 s.
pub fn gpu_status() -> (i32, Option<u32>) {
    let now = Instant::now();
    {
        let c = GPU_STATUS.lock().expect("gpu status");
        if let Some((t, temp, power)) = *c {
            if now.duration_since(t) < Duration::from_secs(2) {
                return (temp, power);
            }
        }
    }
    let fresh = amdgpu_hwmon().or_else(nvidia_status).unwrap_or((-1, None));
    *GPU_STATUS.lock().expect("gpu status") = Some((now, fresh.0, fresh.1));
    fresh
}

/// Card temperature for the search loop's thermal policy.
pub fn read_gpu_temp_c() -> i32 {
    gpu_status().0
}

/// Apply an NVIDIA board power limit (`nvidia-smi -pl W`).
///
/// Runs as the app user first; on a permission denial it escalates just
/// this one command — passwordless `sudo -n` when the admin allowed
/// exactly it (`ALL=(ALL) NOPASSWD: /usr/bin/nvidia-smi -pl *`), else
/// `pkexec` (desktop password prompt) — so the GUI field works without
/// ever running the wallet itself as root. Any other failure is returned
/// as-is (never fatal: the thermal guard still bounds the card).
pub fn apply_power_limit(watts: u32) -> Result<(), String> {
    let arg = watts.to_string();
    let run = |prog: &str, args: &[&str]| -> Result<(), String> {
        match std::process::Command::new(prog).args(args).output() {
            Ok(o) if o.status.success() => Ok(()),
            Ok(o) => {
                let err = String::from_utf8_lossy(&o.stderr).trim().to_string();
                let out = String::from_utf8_lossy(&o.stdout).trim().to_string();
                Err(if err.is_empty() { out } else { err })
            }
            Err(e) => Err(format!("{prog} unavailable ({e})")),
        }
    };
    match run("nvidia-smi", &["-pl", &arg]) {
        Ok(()) => Ok(()),
        Err(e) if is_permission_denied(&e) => {
            if run("sudo", &["-n", "nvidia-smi", "-pl", &arg]).is_ok() {
                return Ok(());
            }
            run("pkexec", &["nvidia-smi", "-pl", &arg])
        }
        Err(e) => Err(e),
    }
}

/// Driver refusal text: only privilege escalation can help these.
fn is_permission_denied(msg: &str) -> bool {
    msg.contains("Insufficient Permissions") || msg.contains("Permission denied")
}

/// A compiled GPU ready to grind LuckyHash.
pub struct GpuSearcher {
    dev: GpuDevice,
    batch: usize,
}

impl GpuSearcher {
    /// Open platform/device and compile the fill kernel. `batch` =
    /// instances per launch (1 MiB VRAM each), clamped to >= 1.
    pub fn open(platform: usize, device: usize, batch: usize) -> Result<Self, String> {
        Ok(Self {
            dev: GpuDevice::open(platform, device)?,
            batch: batch.max(1),
        })
    }

    pub fn name(&self) -> &str {
        self.dev.name()
    }

    pub fn batch(&self) -> usize {
        self.batch
    }

    /// One full LuckyHash through the GPU pipeline. The cross-check
    /// surface for the randomized KAT battery (doc 15 §15.4).
    pub fn hash_nonce(&self, header: &[u8], height: u64, nonce: u64) -> Result<[u8; 32], String> {
        let seed = pow_seed(header, nonce);
        let h0 = argon2_prep::h0(
            &seed,
            &argon2_prep::lucky_salt(height),
            lm_pow::ASSOCIATED_DATA,
        );
        let inits = [argon2_prep::initial_blocks(&h0)];
        let lasts = self.dev.fill_batch(&inits)?;
        Ok(final_digest(&seed, &argon2_prep::mem_from_last(&lasts[0])))
    }

    /// Prep + fill + finalize one batch; first solution in nonce order.
    fn run_batch(
        &self,
        header: &[u8],
        height: u64,
        target: &[u8; 32],
        start: u64,
        n: usize,
    ) -> Result<Option<Solution>, String> {
        let salt = argon2_prep::lucky_salt(height);
        let mut seeds = Vec::with_capacity(n);
        let mut inits = Vec::with_capacity(n);
        for i in 0..n {
            let seed = pow_seed(header, start + i as u64);
            let h0 = argon2_prep::h0(&seed, &salt, lm_pow::ASSOCIATED_DATA);
            inits.push(argon2_prep::initial_blocks(&h0));
            seeds.push(seed);
        }
        let lasts = self.dev.fill_batch(&inits)?;
        for (i, last) in lasts.iter().enumerate() {
            let out = final_digest(&seeds[i], &argon2_prep::mem_from_last(last));
            if meets_target(&out, target) {
                return Ok(Some(Solution {
                    nonce: start + i as u64,
                    out,
                }));
            }
        }
        Ok(None)
    }

    /// Sequential search over `[start, start + count)`, policy-paced
    /// between launches. Deterministic on one device: the lowest matching
    /// nonce wins (CPU search with `threads = 1` agrees).
    pub fn search(
        &self,
        header: &[u8],
        height: u64,
        target: &[u8; 32],
        start: u64,
        count: u64,
        policy: &MiningPolicy,
    ) -> Result<Option<Solution>, String> {
        if count == 0 {
            return Ok(None);
        }
        let mut rt = policy.runtime();
        let end = start.saturating_add(count);
        let mut n = start;
        while n < end {
            let take = (end - n).min(self.batch as u64) as usize;
            let t0 = Instant::now();
            if let Some(sol) = self.run_batch(header, height, target, n, take)? {
                return Ok(Some(sol));
            }
            let d = rt.decide(read_gpu_temp_c(), t0.elapsed());
            if !d.is_zero() {
                std::thread::sleep(d);
            }
            n += take as u64;
        }
        Ok(None)
    }

    /// Timed pipeline run over `hashes` nonces; the same report shape as
    /// the CPU [`crate::bench`] so device-matrix rows compare directly
    /// (`cpu` carries the device name, `threads` is 1 device).
    pub fn bench(&self, header: &[u8], height: u64, hashes: u64) -> Result<BenchReport, String> {
        if hashes == 0 {
            return Err("--hashes must be >= 1".to_string());
        }
        let no_target = [0u8; 32]; // strict less-than: nothing ever validates
        let wall = Instant::now();
        let mut n = 0u64;
        while n < hashes {
            let take = (hashes - n).min(self.batch as u64) as usize;
            self.run_batch(header, height, &no_target, n, take)?;
            n += take as u64;
        }
        let elapsed = wall.elapsed();
        let total_hps = hashes as f64 / elapsed.as_secs_f64().max(1e-9);
        Ok(BenchReport {
            threads: 1,
            hashes_per_thread: hashes,
            total_hashes: hashes,
            elapsed_ms: elapsed.as_millis(),
            total_hps,
            per_thread_hps: vec![total_hps],
            ms_per_hash: 1000.0 / total_hps,
            cpu: self.dev.name().to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_denial_classifier() {
        assert!(is_permission_denied(
            "Failed to set power management limit for GPU 00000000:2B:00.0: Insufficient Permissions"
        ));
        assert!(is_permission_denied("Error: Permission denied"));
        assert!(!is_permission_denied("Invalid power limit 0 W"));
        assert!(!is_permission_denied("nvidia-smi unavailable (not found)"));
        assert!(!is_permission_denied(""));
    }

    /// Hardware gate — run with a GPU present:
    /// `GPU_TESTS=1 cargo test --release -p lm-miner -- --ignored`.
    /// 1024 deterministic pseudo-random (header, nonce, height) cases, one
    /// real batch per 256: the GPU pipeline must equal `lm_pow::pow_hash`
    /// for every one (doc 15 §15.4 randomized battery), which also pins
    /// per-instance seed/fill pairing inside a batch.
    #[test]
    #[ignore]
    fn gpu_battery_matches_lm_pow() {
        if std::env::var("GPU_TESTS").is_err() {
            eprintln!("skipping: set GPU_TESTS=1 with OpenCL hardware present");
            return;
        }
        let g = GpuSearcher::open(0, 0, 256).expect("open default GPU");
        eprintln!("battery on {} (batch {})", g.name(), g.batch());
        let mut state = 0x1234_5678_9abc_def0u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let cases: Vec<(Vec<u8>, u64, u64)> = (0..1024)
            .map(|_| {
                let header: Vec<u8> = (0..80).map(|_| (next() & 0xff) as u8).collect();
                (header, next(), 1 + (next() % 1_000_000))
            })
            .collect();
        for (chunk_i, chunk) in cases.chunks(256).enumerate() {
            let mut seeds = Vec::with_capacity(chunk.len());
            let mut inits = Vec::with_capacity(chunk.len());
            let mut wants = Vec::with_capacity(chunk.len());
            for (header, nonce, height) in chunk {
                let seed = pow_seed(header, *nonce);
                let salt = argon2_prep::lucky_salt(*height);
                let h0 = argon2_prep::h0(&seed, &salt, lm_pow::ASSOCIATED_DATA);
                inits.push(argon2_prep::initial_blocks(&h0));
                seeds.push(seed);
                wants.push(lm_pow::pow_hash(header, *nonce, *height));
            }
            let lasts = g.dev.fill_batch(&inits).expect("gpu fill");
            for (i, last) in lasts.iter().enumerate() {
                let got = final_digest(&seeds[i], &argon2_prep::mem_from_last(last));
                let case = chunk_i * 256 + i;
                assert_eq!(got, wants[i], "case {case}");
            }
        }
    }
}
