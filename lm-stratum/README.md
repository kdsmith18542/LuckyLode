# `lm-stratum` — reference pool wire layer

Testnet-in-a-box: a threaded TCP pool server speaking Stratum V1 framing
with the Lucky Lode job profile (`lowmine/1`), running on a mock chain with
the *real* money rules. No async runtime — std threads only.

- `v1` — protocol types, job registry (4-job window), pure share
  evaluation (accept/stale/dupe/low-diff/unknown-job/bad-nonce with typed
  reject codes 20–24), JSON-RPC shapes, worker→id mapping (FNV-1a).
- `provider` — `TemplateProvider` seam + `MockProvider`: real EmberHash
  verification, real `T/100` jackpot trigger, real levy/bonus/cap machine,
  height advance.
- `rpc` — std-only HTTP JSON-RPC client (no async runtime, no HTTP deps).
- `node_provider` — the production provider: `getblocktemplate` → jobs,
  `submitblock` → chain. Verified live 2026-10-11 against the forked node
  (regtest, EmberHash): two pool-mined shares became blocks 5 and 6.
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
cargo test -p lm-stratum   # unit + e2e (real TCP on localhost)
```

## Mainnet limits (reference pool, pre-launch)

This is the reference implementation; run it on testnet knowing:

- **No persistence.** Shares, balances, worker registry, dedupe, and
  block records are in-memory only. A restart loses them (the ledger's
  own retention bounds memory, not durability). Do not treat balances
  as real until an account store lands.
- **Payouts are v1.** `--payout-threshold/--payout-interval` drive a
  `sendtoaddress` loop from the node wallet (dry-run unless
  `--payout-exec`); worker names double as payout addresses. Not yet
  tracked: coinbase maturity/confirmations (payouts execute on accept —
  sends fail safe with "insufficient funds" while coinbases mature, so
  run a mature chain or lower expectations), persistence across
  restarts, fee splits beyond the per-block skim.
- **No auth, no TLS.** Stratum and the `--http` dashboard are plaintext
  and identify workers by name hash; bind to localhost or a private
  network.
- **Claim modes change block liveness.** `--claim-jackpots` /
  `--claim-pot` serve claim templates and locally withhold non-drawing
  finds (1/100 and 1/1000 draws). That is correct for hunting a claim,
  but it throttles block submission — don't run a public pool that way.
- **Work-identity dedupe is windowed** (2^16 identities). A replay older
  than the window can pass; accepted as the reference trade-off.
- **Stratum V1 framing only** (V2 planned; the provider seam and money
  rules carry over). No `set_difficulty` — targets ride in `mining.notify`.

Tracked follow-ups: persistence + payout processor + auth
(`internal/17`), Stratum V2 framing, `lm-miner pool` subcommand speaking
`client`, attestation-gated league entry.
