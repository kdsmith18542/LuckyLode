# 09 — GPU Posture: Tripwire, Levers, Measurement (policy, not spec)

**Status:** maintainer policy, 2026-10-11. Tunable by review; it constrains
*when we act*, never consensus rules. Touches nothing frozen (doc 05 rev 1.0
stands: EmberHash v1 genesis at 1 MiB).

## Posture in one sentence

Hold the 1 MiB genesis for maximal low-end inclusion; GPUs race the top
league under sub-linear rewards meanwhile; a sustained 10× efficiency breach
pulls a pre-committed lever at the next *scheduled* upgrade — never an
emergency fork debate.

## Why 1 MiB accepts GPU pressure (accepted trade, not oversight)

One evaluation moves ~6 MB through memory (1 MiB × read+write × 3 passes),
so GPUs run thousands of concurrent instances and hide latency — the thing
they're built for. Estimated steady state: ~25 H/s/W (phone big core),
~45 H/s/W (desktop thread), ~350 H/s/W (midrange GPU) — roughly a 10× edge
over low-end hardware. The footprint that causes this is the same footprint
that lets decade-old phones and 512 MB boards mine at all. That trade is the
project's identity (Lucky Lode, not phonecoin, not GPU-coin): inclusion first,
levers loaded.

## Tripwire (the only number that compels action)

- **Metric:** sustained hashes/second/watt, measured quarterly per doc 02 §2.2.
- **Denominator:** the most efficient *eligible* low-end reference box (top
  of the laptop/desktop league, e.g. N100-class miniPC) — **not** the phone.
  Phones are protected structurally by league separation (doc 03 §3.3) and
  sub-linear splits (doc 06 §6.3); phone-vs-GPU ratios are telemetry, never
  a trigger.
- **Threshold:** GPU efficiency > **10×** the reference box, sustained over
  a full quarterly window (single-quarter spikes don't fire).
- **On breach:** the L1 lever below ships in the next scheduled upgrade.
  No new debate, no spec reopen — this document *is* the pre-authorization.

## Low-end GPU support (added 2026-10-11): supported miners, not just tolerated
competitors

CPUs first was the launch posture; the supported set now explicitly
includes **low-end GPUs: Pascal-class discrete cards, AMD GCN/RDNA
low-end, and Intel/AMD iGPUs**. Rationale: "whatever hardware you already
own" includes a 2016 GTX 1060 and every laptop iGPU — and EmberHash's
1 MiB footprint (vs 4 GB+ DAGs) is precisely what makes that class
*includable* instead of gated out. This changes nothing about the
tripwire above: it guards the efficiency *ratio*, and leagues still
separate phones from farms by observed hashrate.

- **One codebase: OpenCL.** A single OpenCL Argon2id kernel covers NVIDIA
  Pascal (OpenCL 1.2+), AMD low-end, and both iGPU vendors — no CUDA
  lock-in, no per-vendor miners. iGPUs share the package thermal budget
  with the CPU, so the lm-sched thermal policy (doc 07) applies doubly.
- **Split design: GPU does Argon2id (~95% of per-hash cost), CPU keeps the
  two BLAKE3 hashes.** Minimal new code for most of the gain; the submit
  path (Stratum) is unchanged.
- **Honest numbers posture:** expect efficiency (H/J) wins over raw-rate
  miracles on iGPUs (shared memory bandwidth caps them); measure per
  device, publish in the matrix, no promises before numbers.
- **Spike plan (P2/P3, not started):** (1) OpenCL Argon2id kernel,
  correctness-gated against the frozen KATs; (2) per-device bench rows;
  (3) `lm-miner --device opencl` integration; (4) thermal interplay tests
  (iGPU + dGPU + CPU under one scheduler).

## Response ladder (in order, each step only if the last proved insufficient)

- **L1 — `m_cost` 1→4 MiB.** Pre-committed first pull. No eligible device
  notices 4 MiB; GPUs lose parallel instances roughly linearly. Verification
  stays trivial. Parameter-only change, no construction touch.
- **L2 — `t_cost` up.** Primarily an ASIC deterrent (area–time); weak
  against GPUs per-watt and taxes phones directly. Use only with measured
  ASIC evidence, not GPU pressure.
- **L3 — construction reopen (v2 candidate).** Serial AES tail off the
  Argon2 output: unparallelizable latency work both AES-NI desktops and
  ARMv8-crypto phones execute well (equalization, the VerusHash insight).
  Requires new KATs, new audit, new bake-in — a decision record of its own,
  contingent on measured L1 failure.
- **Backstop — 90-day fork policy** (docs 02 §2.2, 04 §4.5) for ASIC
  appearance. Unchanged.

## Measurement (so the tripwire has data, not anecdotes)

- Testnet week 1: borrowed-GPU baseline rows in the device matrix
  (doc 05 §5.6), same `lm-miner bench --json` format as every other row.
- Quarterly publication of class-efficiency ratios alongside the matrix.
- Re-measure the reference low-end box on the same cadence — the
  denominator moves as hardware improves; a 2026 N100 is not a 2029 one.

## Explicit non-triggers

- Phone-vs-GPU ratios at any value (league cover).
- Single-quarter GPU spikes (sustained-window rule).
- GPU presence in the top league per se (security subsidy while small).
- Absolute H/s comparisons across algorithms (difficulty-neutral theater).
