# `lm-luckynode` — Luckynode participation over node RPC

Shared flows for entering the consensus Luckynode Draw (doc 06 §6.7)
without any pool or local key files. Everything spends from the
connected node's wallet.

- **Identity**: `ensure_identity` drives `registernamecommitment` →
  confirmation → `registeridentity`, resuming an in-flight commitment
  instead of minting another fee-paying one. `ln-` namespaces only.
- **Heartbeats**: `heartbeat_if_due` updates the identity when the epoch
  crosses (or liveness lapses), and skips re-submission until the tip
  advances (idle chains would otherwise accumulate duplicate txs).
- **Status**: `identity_status` + `resolve_epoch_len` report the live
  draw window and eligibility.

Used by the desktop wallet's Luckynodes tab and the standalone
`luckynode` daemon. The daemon additionally persists its pending
commitment and last-heartbeat tip in a state file (atomic writes,
fail-closed on corruption).

```sh
cargo test -p lm-luckynode
```
