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
    /// Persistent launch buffers for `batch` instances: created lazily on
    /// the first grind batch and reused for every launch after
    /// (per-launch clCreateBuffer of a batch x 1 MiB mem buffer was
    /// driver churn).
    bufs: std::sync::Mutex<Option<crate::opencl::BatchBuffers>>,
}

impl GpuSearcher {
    /// Open platform/device and compile the fill kernel. `batch` =
    /// instances per launch (1 MiB VRAM each), clamped to >= 1.
    pub fn open(platform: usize, device: usize, batch: usize) -> Result<Self, String> {
        Ok(Self {
            dev: GpuDevice::open(platform, device)?,
            batch: batch.max(1),
            bufs: std::sync::Mutex::new(None),
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

    /// Prep one batch (CPU side of the pipeline).
    fn prep_batch(
        &self,
        header: &[u8],
        height: u64,
        start: u64,
        n: usize,
    ) -> (Vec<[u8; 32]>, Vec<[[u64; 128]; 2]>) {
        let salt = argon2_prep::lucky_salt(height);
        let mut seeds = Vec::with_capacity(n);
        let mut inits = Vec::with_capacity(n);
        for i in 0..n {
            let seed = pow_seed(header, start + i as u64);
            let h0 = argon2_prep::h0(&seed, &salt, lm_pow::ASSOCIATED_DATA);
            inits.push(argon2_prep::initial_blocks(&h0));
            seeds.push(seed);
        }
        (seeds, inits)
    }

    /// Finalize one completed batch; first solution in nonce order.
    fn finalize_batch(
        seeds: &[[u8; 32]],
        lasts: &[[u64; 128]],
        target: &[u8; 32],
        start: u64,
    ) -> Option<Solution> {
        for (i, last) in lasts.iter().enumerate() {
            let out = final_digest(&seeds[i], &argon2_prep::mem_from_last(last));
            if meets_target(&out, target) {
                return Some(Solution {
                    nonce: start + i as u64,
                    out,
                });
            }
        }
        None
    }

    /// Pipelined batch loop (search + bench share it): submit batch N,
    /// wait on batch N-1 and finalize it while the GPU fills N — the GPU
    /// never idles between fills while the CPU preps/finalizes (~22% duty
    /// cycle recovered over the serialized loop).
    ///
    /// `on_batch` sees every completed batch; returning true stops the
    /// loop (a found solution for search).
    #[allow(clippy::too_many_arguments)] // one pipeline signature shared by search + bench
    fn pipelined_batches(
        &self,
        header: &[u8],
        height: u64,
        target: &[u8; 32],
        start: u64,
        end: u64,
        mut on_batch: impl FnMut(Option<Solution>, Instant) -> bool,
        rt: &mut crate::thermal::PolicyRuntime,
    ) -> Result<(), String> {
        let mut n = start;
        // (pending batch, its seeds, its start nonce, batch wall clock)
        let mut in_flight: Option<(crate::opencl::PendingBatch, Vec<[u8; 32]>, u64, Instant)> =
            None;
        while n < end || in_flight.is_some() {
            if n < end {
                let take = (end - n).min(self.batch as u64) as usize;
                let (seeds, inits) = self.prep_batch(header, height, n, take);
                let t0 = Instant::now();
                let pending = {
                    let mut guard = self.bufs.lock().expect("gpu buffers");
                    if guard.is_none() {
                        *guard = Some(self.dev.create_batch_buffers(self.batch)?);
                    }
                    self.dev
                        .submit_batch(guard.as_ref().expect("just created"), &inits)?
                };
                let mut stop = false;
                if let Some((prev, prev_seeds, prev_start, prev_t0)) = in_flight.take() {
                    let lasts = prev.wait_and_take()?;
                    let sol = Self::finalize_batch(&prev_seeds, &lasts, target, prev_start);
                    if on_batch(sol, prev_t0) {
                        stop = true;
                    }
                }
                if stop {
                    // The just-submitted batch is still in flight: drain it
                    // before returning (its host buffers must outlive the
                    // queue's pending commands).
                    if let Some((pending, ..)) = in_flight.take() {
                        let _ = pending.wait_and_take()?;
                    }
                    return Ok(());
                }
                in_flight = Some((pending, seeds, n, t0));
                let d = rt.decide(read_gpu_temp_c(), t0.elapsed());
                if !d.is_zero() {
                    std::thread::sleep(d);
                }
                n += take as u64;
            } else {
                // Drain the last in-flight batch.
                let (prev, prev_seeds, prev_start, prev_t0) = in_flight.take().expect("drain");
                let lasts = prev.wait_and_take()?;
                let sol = Self::finalize_batch(&prev_seeds, &lasts, target, prev_start);
                on_batch(sol, prev_t0);
            }
        }
        Ok(())
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
        let mut found: Option<Solution> = None;
        self.pipelined_batches(header, height, target, start, end, |sol, _| {
            if sol.is_some() {
                found = sol;
                true // stop the pipeline: first find wins
            } else {
                false
            }
        }, &mut rt)?;
        Ok(found)
    }

    /// Timed pipeline run over `hashes` nonces; the same report shape as
    /// the CPU [`crate::bench`] so device-matrix rows compare directly
    /// (`cpu` carries the device name, `threads` is 1 device).
    pub fn bench(&self, header: &[u8], height: u64, hashes: u64) -> Result<BenchReport, String> {
        if hashes == 0 {
            return Err("--hashes must be >= 1".to_string());
        }
        let no_target = [0u8; 32]; // impossible target: nothing ever validates
        let wall = Instant::now();
        let mut rt = MiningPolicy::unguarded().runtime();
        self.pipelined_batches(
            header,
            height,
            &no_target,
            0,
            hashes,
            |_sol, _| false,
            &mut rt,
        )?;
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

#[cfg(test)]
mod perf_probe {
    //! Dev-only timing probes (ignored): measure the CPU share of the
    //! GPU pipeline to decide where overlap pays.
    use super::*;
    use std::time::Instant;

    #[test]
    #[ignore]
    fn cpu_prep_finalize_share_of_batch() {
        let n = 512usize;
        let header = [7u8; 80];
        let height = 100u64;
        let target = [0u8; 32]; // impossible: nothing hits, timing only
        let salt = argon2_prep::lucky_salt(height);
        let t0 = Instant::now();
        let mut seeds = Vec::with_capacity(n);
        let mut inits = Vec::with_capacity(n);
        for i in 0..n {
            let seed = pow_seed(&header, i as u64);
            let h0 = argon2_prep::h0(&seed, &salt, lm_pow::ASSOCIATED_DATA);
            inits.push(argon2_prep::initial_blocks(&h0));
            seeds.push(seed);
        }
        let t_prep = t0.elapsed();
        let t1 = Instant::now();
        let lasts = vec![[0u64; 128]; n]; // fake fills: finalize only
        let mut hits = 0;
        for (i, last) in lasts.iter().enumerate() {
            let out = final_digest(&seeds[i], &argon2_prep::mem_from_last(last));
            if meets_target(&out, &target) {
                hits += 1;
            }
        }
        let t_fin = t1.elapsed();
        println!("prep 512: {:.1} ms ({:.3} ms/inst)", t_prep.as_secs_f64()*1000.0, t_prep.as_secs_f64()*1000.0/n as f64);
        println!("finalize 512: {:.1} ms ({:.3} ms/inst)", t_fin.as_secs_f64()*1000.0, t_fin.as_secs_f64()*1000.0/n as f64);
        assert_eq!(hits, 0);
    }
}
