# `lm-pow` — LuckyHash v1 reference implementation

Reference (normative) implementation of the Lucky Lode proof-of-work, frozen
in [`../05-pow-algorithm.md`](../05-pow-algorithm.md) rev 1.0.

- Library API: `pow_hash(header, nonce, height) -> [u8; 32]`,
  `pow_full(...)` (seed/mem/out stages), `meets_target(out, target)`;
  `difficulty::{lwma_next_target, compact_to_target, target_to_compact}`
  (exact integer LWMA + Bitcoin-compatible nBits, doc 05 §5.3);
  jackpot trigger `difficulty::{jackpot_threshold, is_jackpot}` +
  crate-root `is_jackpot` (`out < floor(T/100)`, doc 06 §6.1).
- `tests/kat.rs`: frozen known-answer vectors (doc 05 §5.4);
  `tests/difficulty.rs`: oracle-checked LWMA/nBits vectors (§5.3.1).
  **These must never be edited to match code** — a mismatch is a bug here.
- `benches/pow.rs`: single-hash criterion bench (per-device-class
  measurement for doc 05 §5.6 starts here).
- Siblings: `../lm-rewards` (jackpot split, doc 06),
  `../lm-sched` (thermal scheduler, doc 07).

```sh
cargo test -p lm-pow        # unit + KAT suite
cargo bench -p lm-pow       # single-hash timing
```

Status: portable reference. NEON-tuned kernels are a tracked optimization
(doc 05 §5.7) — merged only with measured uplift on the device matrix.
