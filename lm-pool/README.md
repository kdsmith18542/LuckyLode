# `lm-pool` — pool-side fun-layer accounting

Pure-logic reference for the retention engine (docs 03, 06). No I/O, no
networking, no consensus writes — it reads verified shares and produces
payouts + leaderboards that replay deterministically from the log.

- `streak` — uptime streaks (+1%/day to +30%, 48 h reset), comeback
  (+10% × 7 days after ≥14 days idle), overnight (+5% per flagged share).
- `league` — device-class classification from observed rate
  (phone <10, miniPC 10–60, desktop 60–500 MH/s; farm above, paid but
  prize-ineligible).
- `ledger` — PPLNS share log, effective-share aggregation
  (`work × overnight × comeback`, then `× streak`), jackpot payouts via
  `lm-rewards` sqrt/cap/dust, linear league-pool splits, seasonal board.
  **Bounded history** (review finding G): raw shares are retained for 32
  days (the longest supported query window); older history is rolled up
  into per-miner UTC active-day lists plus a per-miner comeback state, so
  streaks and comeback weights survive pruning at O(1) memory per miner.
  Query windows longer than the retention see the retained tail only.

```sh
cargo test -p lm-pool   # unit + season-simulation tests
```

`tests/season.rs` runs a 30-day story (phone loyalist, weekend warrior,
comeback return, farm) pinning every number end to end.

Tracked follow-ups: Stratum V2 server/client speaking this ledger,
attestation-gated league entry (doc 03 §3.6), stake-to-enter prize tiers.
