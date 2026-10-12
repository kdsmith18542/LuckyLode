//! GPU (OpenCL) EmberHash backend: doc 15's split as a drop-in search.
//!
//! CPU does both BLAKE3s, the Argon2 prep (H0 + initial blocks), and the
//! H' finalize; the GPU runs only the memory-hard fill (doc 15 §15.1).
//! Results are byte-identical to [`lm_pow::pow_full`] — the KAT gate
//! holds the kernel to that — and the 1080 is 5-10x this box's CPU
//! (doc 15 §15.7).
//!
//! Thermal note: `--max-temp` reads the CPU package sensor. The GPU's
//! own temperature/power guard is doc 15 §15.5 (step 3, not wired yet) —
//! until then keep GPU runs bounded and watch the card.

use std::time::Instant;

use lm_pow::{final_digest, meets_target, seed as pow_seed};

use crate::argon2_prep;
use crate::opencl::GpuDevice;
use crate::thermal::{self, MiningPolicy};
use crate::{BenchReport, Solution};

/// Default Argon2 instances per launch: 512 MiB of VRAM (1 MiB each).
pub const DEFAULT_BATCH: usize = 512;

/// A compiled GPU ready to grind EmberHash.
pub struct GpuSearcher {
    dev: GpuDevice,
    batch: usize,
}

impl GpuSearcher {
    /// Open platform/device and compile the fill kernel. `batch` =
    /// instances per launch (1 MiB VRAM each), clamped to >= 1.
    pub fn open(platform: usize, device: usize, batch: usize) -> Result<Self, String> {
        Ok(Self { dev: GpuDevice::open(platform, device)?, batch: batch.max(1) })
    }

    pub fn name(&self) -> &str {
        self.dev.name()
    }

    pub fn batch(&self) -> usize {
        self.batch
    }

    /// One full ember hash through the GPU pipeline. The cross-check
    /// surface for the randomized KAT battery (doc 15 §15.4).
    pub fn hash_nonce(&self, header: &[u8], height: u64, nonce: u64) -> Result<[u8; 32], String> {
        let seed = pow_seed(header, nonce);
        let h0 = argon2_prep::h0(&seed, &argon2_prep::ember_salt(height), lm_pow::ASSOCIATED_DATA);
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
        let salt = argon2_prep::ember_salt(height);
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
                return Ok(Some(Solution { nonce: start + i as u64, out }));
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
            let d = rt.decide(thermal::read_temp_c(), t0.elapsed());
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
                let salt = argon2_prep::ember_salt(*height);
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
