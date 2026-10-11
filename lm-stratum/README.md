# `lm-stratum` — reference pool wire layer

Testnet-in-a-box: a threaded TCP pool server speaking Stratum V1 framing
with the Lowmine job profile (`lowmine/1`), running on a mock chain with
the *real* money rules. No async runtime — std threads only.

- `v1` — protocol types, job registry (4-job window), pure share
  evaluation (accept/stale/dupe/low-diff/unknown-job/bad-nonce with typed
  reject codes 20–24), JSON-RPC shapes, worker→id mapping (FNV-1a).
- `provider` — `TemplateProvider` seam (the forked node implements this
  over its RPC in P1) + `MockProvider`: real EmberHash verification, real
  `T/100` jackpot trigger, real levy/bonus/cap machine, height advance.
- `server` — `spawn_server(provider, "127.0.0.1:0")`: per-connection
  threads, per-connection **vardiff** (20 s/share target, 8-share window,
  4×-clamped steps, conn-local jobs at `clean=false`), job broadcast on
  template change, ledger shares, league-pool credits, jackpot splits,
  block records, counters.
- `client` — blocking client (id-matched responses, buffered notifies).

Wire profile (`mining.notify` params):
`[job_id, header_hex, height, pool_target_hex, net_target_hex, clean]`;
submit: `[worker, job_id, nonce_hex]`. Documented deviations from
public-pool V1: empty extranonce, no `set_difficulty` (targets ride in
notify), no per-share overnight flag yet.

```sh
cargo test -p lm-stratum   # 16 unit + 3 e2e (real TCP on localhost)
```

Tracked follow-ups: Stratum V2 framing (provider seam and money rules
carry over), `lm-miner pool` subcommand speaking `client`, vardiff,
attestation-gated league entry.
