# `lm-miner` — reference EmberHash v1 miner (solo + benchmark)

Portable-first CPU/ARM miner core + binary. One Argon2id instance per
thread (`p = 1` scales across threads). No third-party CLI or networking
deps by design — builds offline, easy to audit.

```sh
# Benchmark mode (doc 05 §5.6 device-matrix rows: --json for collection)
cargo run -p lm-miner -- bench --threads 4 --hashes 10
cargo run -p lm-miner -- bench --threads 4 --hashes 10 --json

# Solo search (testnet primitive; pools speak Stratum V2 later)
cargo run -p lm-miner -- mine --header <HEX> --height 1 \
  --target nbits:1f00ffff --threads 4 --count 1000000

# Pool mining against the reference pool (testnet-in-a-box — terminal 1
# serves the mock chain, terminal 2 mines it; every share is a block)
cargo run -p lm-stratum --bin pool_server -- --bind 127.0.0.1:3333
cargo run -p lm-miner -- pool --pool 127.0.0.1:3333 --worker rig1 \
  --threads 4 --max-shares 10
```

`--target` accepts 64-hex-char big-endian, `nbits:XXXXXXXX`
(Bitcoin compact, e.g. the doc 05 §5.3 `POW_LIMIT`), or `max`.
Exit codes: 0 = bench done / solution found, 1 = budget exhausted,
2 = usage error.

Tests: `cargo test -p lm-miner` — lib unit (empty work, bench counts,
JSON shape), `tests/search.rs` (single-thread solve pinned at KAT V1
nonce 0, strict-less boundary, multi-thread validity, stride
determinism), `tests/cli.rs` (flags, exit codes, output shapes).

Tracked follow-ups (doc 02 §2.3): Stratum V2 pool client, `lm-sched`
thread-count hookup + Android JNI, NEON-tuned kernels on measured uplift.
