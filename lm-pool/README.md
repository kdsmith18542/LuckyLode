# `lm-pool` — pool-side fun-layer accounting

Pure-logic reference for the retention engine (docs 03, 06). No I/O, no
networking, no consensus writes — it reads verified shares and produces
payouts + leaderboards that replay deterministically from the log.

- `streak` — uptime streaks (+1%/day to +30%, 48 h reset), comeback
  (+10% × 7 days after ≥14 days idle), overnight (+5% per flagged share).
- `division` — device-class Divisions: the miner's **declared** family
  (phone/cpu/gpu/farm) from its agent string, policed by trailing rate
  (farm-rate claims override; contradicted labels fall back to the
  rate-implied family). No MH/s bands — LuckyHash runs at kH/s.
- `ledger` — PPLNS share log, effective-share aggregation
  (`work × overnight × comeback`, then `× streak`), block-value splits
  (linear pro-rata over the last 2,000 shares), jackpot/pot splits via
  `lm-rewards` sqrt/cap/dust, seasonal Divisions boards.
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

Shipped alongside this ledger: the Stratum V1 + V2 wire layer
(`lm-stratum`) and the payout processor. Attestation flags decorate
Division boards (`/api/attest`, self-declared; no verifier yet).
Pool-side prize money and draws were retired in favour of the consensus
Prize Fund (doc 06 §6.7) — this crate holds no game-money balances.
