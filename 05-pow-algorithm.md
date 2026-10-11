# 05 — Proof-of-Work Algorithm: EmberHash v1 (working name, FROZEN rev 1.0)

**Status:** spec frozen 2026-10-10. Test vectors below are real outputs of
the pinned construction (generated with `argon2` 0.6 + `blake3` reference
crates). Name is provisional — community rename contest may replace it
without touching parameters.

**Decision record:** the project launches with a purpose-built phone-ideal
algorithm at genesis (not a VerusHash-compatible stage). Rationale: one
algorithm means one identity, one benchmark story, and no wasted migration;
the added risk is contained by conservative parameters, an 8+-week testnet
mining these exact parameters pre-genesis, and a no-mainnet gate on
third-party review (§5).

## 5.1 Construction

```
seed = BLAKE3(header_bytes || nonce_le64)            # 32 B, binds all header data
salt = "LMv1" || height_le64                         # 12 B, binds block height
mem  = Argon2id(seed, salt, AD="lowmine-pow/v1",
                m=1024 KiB, t=3, p=1, out=32)        # the memory-hard pass
out  = BLAKE3(mem || seed)                           # 32 B final digest
valid ⇔ uint256_be(out) < target                     # target from nBits (§5.3)
```

Reference: Argon2 v0x13, BLAKE3 (2020-01-04+ behavior). All integers
little-endian except the final target comparison (big-endian, Bitcoin
convention). Single-lane (`p=1`): miners scale by running one instance per
thread (as Chukwa did), keeping per-instance verification cheap for phones
and RPis syncing the chain.

## 5.2 Frozen parameters

| Param | Value | Why |
|---|---|---|
| KDF | Argon2id, version 0x13 | PHC winner; hybrid side-channel resistance; audited implementations in C and Rust |
| `m_cost` | 1024 (1 MiB) | Fits phone L2/L3 + old devices (Chukwa proved 1 MB viable); keeps sync cheap |
| `t_cost` | 3 | ~1.5 ms/hash desktop-class x86; est. 5–15 ms phone big core (to be measured §5.6) |
| `p_cost` | 1 | One lane per instance; no intra-hash parallelism to port or verify |
| Salt | `"LMv1" \|\| height_le64` | Height-binding kills cross-height precomputation/replay; `LMv1` versions the construction |
| AD | `"lowmine-pow/v1"` (14 B, ≤32 B limit) | Domain separation: our hashes are ours alone |
| Pre/post hash | BLAKE3 | Binds header/nonce (pre) and breaks Argon2 output structure (post); fast on ARMv8 |
| Block time `T` | 60 s | Frequent small wins = fun; verification load trivial |
| Retarget | LWMA, N=60, every block (§5.3) | Fast adaptation so farm spikes don't strand phones |
| Target encoding | Bitcoin `nBits` compact | Existing pool/stratum tooling parses it unchanged |

**Tuning levers reserved (no fork needed to discuss, fork needed to turn):**
`t_cost` up (GPU pressure), `m_cost` up (ASIC pressure, at the cost of old
phones — the dial that trades inclusivity for resistance, set explicitly).

## 5.3 Difficulty rules (integer math, deterministic — rev 1.1)

Exact algorithm (reference: `lm-pow::difficulty`; any deviation is a
consensus bug):

```text
N = 60 blocks, T = 60 s, K = N(N+1)/2 = 1830
inputs: targets[0..60] oldest→newest (U256), times[0..60] unix stamps
        (times[0] parents the first block)
acc  = Σ clamp(times[i]-times[i-1], 0, 6T) * i,  i = 1..=60
       (saturating subtraction; recent blocks weigh most)
avg  = Σ targets / 60                                    (floor)
raw  = avg * acc / (T * K)                               (floor; U512
       intermediate, saturate to U256::MAX — unreachable: needs avg > 2^256/6)
next = clamp(raw, last - last/3, last + last/3)          (floor legs, ±33%)
next = min(next, POW_LIMIT)
```

- `POW_LIMIT` (easiest allowed target) = `0x1f00ffff` expansion =
  `0x00FFFF0000…0000` (31 bytes). Block targets above it are invalid.
- `nBits` compact is Bitcoin-exact (exponent/mantissa + sign-bit rule);
  negative, zero-mantissa, and overflowing (>256-bit) encodings are
  invalid. Roundtrip anchor: `0x1d00ffff` (Bitcoin genesis) and
  `0x1f00ffff` (our limit).
- The ±33% clamp doubles as the degenerate-timestamp floor: all-equal
  timestamps compute `raw = 0` and land exactly on `last − last/3`.
- Genesis `nBits` targets **10 MH/s network equivalent** (easy start for a
  fair launch; exact value frozen at testnet).
- Timestamp rule: block time must exceed median-time-past of the prior 11
  and may not exceed network time + 2 h (Bitcoin rules, boring on purpose).

### 5.3.1 Difficulty vectors (normative — full inputs in `lm-pow/tests/`)

- **V1** (all targets `2^220`, all solvetimes 60 s) → exactly `2^220`
  (fixed point).
- **V2** (same targets, all solvetimes 120 s) → `2^220 + 2^220/3`
  (`0x1555…5555`, the +33% clamp binding).
- **V3** (same targets, all solvetimes 30 s) → `2^220 − 2^220/3`
  (`0xAAAA…AAAB`, the −33% clamp binding).
- **V4** (targets `2^220 − i·2^212`, times `60i + 13·(i mod 7)`) →
  `0x000000000eda080ec5bfdfc4e90080ec5bfdfc4e90080ec5bfdfc4e90080ec5b`
  (corrected 2026-10-11: earlier transcription dropped the leading zeros
  and one digit; the Rust `lm-pow` test and the C++ gate agree on the
  value above).
- `compact(0x1d00ffff) = 0x00000000FFFF00…00`; `compact(0x1f00ffff) =
  POW_LIMIT` roundtrip; `0x1c800000`, `0x02808000`, `0x2200ffff` invalid.

## 5.4 Test vectors (normative — header = bytes 0x00..0x4F, 80 bytes)

Generated 2026-10-10, `argon2 0.6 / blake3`, release build:

```
V1  nonce=0  height=1
  seed = c0d0cb7a3009c163ef6d92fa2626158077777553b794c8fa6f3d29a7c7f26e86
  mem  = 446765c0245c7d3da7185456343055971444c45dd90b015d4fd7b3c6262a2b99
  out  = ff0d88d9bf49d0d5c47a78b9a8f5478640359c4da7e73ee23973d6c338e2ca6e

V2  nonce=1  height=1
  seed = 88c16e6cb829970c72730f29ccad9448b425727340f492a47885357f95c6d549
  mem  = 3924a6a0bfefe126c881fc7764794d6f45b992f5f22d33b2fd339af5bf405dbe
  out  = 9c50add868c939b69096279e08387ae4593524d095cdd83a4ef5ea09908c67fd

V3  nonce=18446744073709551615 (u64::MAX)  height=12345
  seed = ee822cda19fa6523becfdaa72ac7a8581ac9316b340f9ca7648c7c8b45777cef
  mem  = 48063374526971416c8c7d8f7b4e4ab815d4e4c6a4956024d2c3b459817a2791
  out  = 335c53228b27bfd9b867774886df4226246727913d6ed8a7933c019b93cf24a0
```

V3 pins the nonce/height edge (max u64 nonce, non-trivial height → salt and
AD paths exercised). Any implementation disagreeing on any byte is wrong.

## 5.5 Fairness & adversary posture (honest)

- **Phone parity estimate:** desktop ~1.5 ms/hash implies a phone big core
  ~5–15 ms/hash (3–10×). LITTLE cores worse per thread but numerous.
  Measurement, not modeling, decides: the testnet ships a benchmark mode
  reporting per-device-class rates (§5.6). Target: no device class slower
  than ~20× the fastest per thread.
- **GPUs: tolerated, bounded, watched.** Argon2id at 1 MiB is GPU-mineable
  (unlike 2 GB RandomX). Expected GPU edge: low single-digit multiples per
  watt, not two orders of magnitude — and GPUs race in the top league only
  (doc 03). If measured GPU efficiency exceeds ~10× flagship-phone
  efficiency per watt, the reserved lever is `t_cost` up at the next
  scheduled upgrade. 90-day fork policy applies (doc 02 §2.2).
- **ASICs:** 1 MB Argon2id is ASIC-able in principle; uneconomical while the
  coin is small, which is the only honest claim. `m_cost` is the dial.
- **Botnets:** unchanged from doc 04 §4.5 — CPU parity cuts both ways.
- **Verification DoS:** one block verifies with one hash (~ms); header sync
  is cheap on any device. No unbalanced-verification trap.
- **No precomputation:** height-bound salt + per-nonce seed; rainbow tables
  inapplicable (unique salt per height).

## 5.6 Measurement plan (testnet, before mainnet)

1. Benchmark mode in the reference miner reports per-thread/per-device
   rates for a pinned device matrix (2 phones, miniPC, old laptop, desktop).
2. Publish phone-vs-desktop ratios; **gate:** worst class ≤ ~20× best per
   thread, else retune `t_cost`/`m_cost` before genesis.
3. GPU watch from week 1: invite GPU miners openly, measure efficiency
   ratios, publish quarterly (doc 02 §2.2).

## 5.7 Audit-readiness & mainnet gates (no exceptions)

- Known-answer tests: §5.4 vectors + edge cases (empty header, zero/high
  nonce, height 0 vs 1 divergence) in every implementation (Rust ref, C
  port if any, pool verifiers).
- Cross-architecture determinism CI: x86_64 + aarch64 runners must agree
  byte-for-byte on all vectors (endianness discipline §5.1).
- Third-party review of construction + parameters before mainnet; **no
  mainnet with open High/Critical findings** (same bar as our Security
  Plan gates). Testnet mines these exact parameters ≥ 8 weeks first.
- Reference implementation plan: Rust crate `lm-pow`
  (`pow_hash(header, nonce, height)`, verify, criterion benches, KAT suite);
  portable first, NEON-tuned kernels as a tracked optimization (measured
  uplift required to merge — no assembly on vibes).

## Addendum A — Fork header layout (P1, does not change rev 1.0)

**Status:** fixes the fork-adapter interface, so the
Verus fork and every external implementation agree on `header_bytes`.
Rev 1.0's construction is untouched: this addendum defines only *what goes
into* `header_bytes` for fork blocks. New vectors below are real outputs.

**Naming note (2026-10-11):** the project was renamed **Lucky Lode**. The
construction strings `LMv1` and `lowmine-pow/v1` predate the rename and
are **frozen consensus identifiers** — they do not follow branding and
never change (the KAT vectors depend on them byte-for-byte).

```text
header_bytes (fork blocks) :=
  nVersion              u32 LE
  hashPrevBlock         32 B (internal/serialized order)
  hashMerkleRoot        32 B
  hashFinalSaplingRoot  32 B
  nTime                 u32 LE
  nBits                 u32 LE
  CompactSize(len(nSolution)) || nSolution bytes
nonce_le64 := CPOSNonce bytes[0..8] (the u64 grind domain, LE)
height     := validation context (CheckProofOfWork parameter; never serialized)
```

Validity rule (fork): PoW blocks require `CPOSNonce` bytes `[8..32] == 0`
(the nonce is exactly the u64 miners grind); PoS blocks are exempt from the
EmberHash target check and keep full `CPOSNonce` semantics. Empty solution ⇒ Length 109 for a minimal PoW header.

Frozen vector `A1` (doc 05 §5.4 style — full inputs in
`lm-pow/examples/fork_vector.rs`, asserted in `lm-pow/tests/fork_layout.rs`):

```
header = 04000100 1111…11(32) 2222…22(32) 3333…33(32) 2a000000 ffff001d 00
nonce=7 height=100
seed = df2f1c48bdc8f58fb4f7247b5c4fe2257011c8842e3859fc245cd654a8030619
mem  = d68a955d887a8e4d4c40d34ec2bc4fbdf8e8f4135e0e856dd6d6a2f957b8110d
out  = e491f172e0d0adcb161fb264413292bfca631fff8c390218b6cabdac6f452131
```

Any C++ `ember_hash` port disagreeing on any byte fails the KAT gate
before review.
