# `lm-sched` — thermal/power-aware mining scheduler

Pure policy for deciding whether, how hard, and with how many threads to
mine on a battery-powered or thermally constrained device (doc 07).

Inputs are a `Snapshot` (battery temperature/level/charging, wall power,
overnight flag, user thresholds); output is a `Decision` (mine/pause,
thread count, reason). No I/O — the Android service and any embedder
feed it real sensor data.

- Overheat latch: once the temperature cap is crossed, a sensor dropout
  cannot silently resume mining.
- Overnight/charging biases and user caps are explicit; zero derate
  bands are rejected.
- Packed decision format for the JNI boundary is pinned in tests on both
  the Rust and Kotlin sides.

```sh
cargo test -p lm-sched
```
