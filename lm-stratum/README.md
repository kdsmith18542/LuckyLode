# `lm-stratum` — reference pool wire layer

Testnet-in-a-box: a threaded TCP pool server speaking Stratum V1 framing
with the Lucky Lode job profile (`luckylode/1`), running on a mock chain with
the *real* money rules. No async runtime — std threads only.

- `v1` — protocol types, job registry (4-job window), pure share
  evaluation (accept/stale/dupe/low-diff/unknown-job/bad-nonce with typed
  reject codes 20–24), JSON-RPC shapes, worker→id mapping (FNV-1a).
- `provider` — `TemplateProvider` seam + `MockProvider`: real LuckyHash
  verification, real `T/100` jackpot trigger, real levy/bonus/cap machine,
  height advance.
- `rpc` — std-only HTTP JSON-RPC client (no async runtime, no HTTP deps).
- `node_provider` — the production provider: `getblocktemplate` → jobs,
  `submitblock` → chain. Verified live 2026-10-11 against the forked node
  (regtest, LuckyHash): two pool-mined shares became blocks 5 and 6.
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

- **Persistence is opt-in.** `--state-file PATH` snapshots the payout
  ledger (owed/paid/fees), worker-name map and block history (capped at
  1000) atomically every 10 s and restores them on boot; delete the file
  to intentionally clear. Still in-memory only: the PPLNS share window,
  worker rates and the dedupe ring — after a restart, the next blocks
  split across post-restart shares, and a very recent replay window is
  forgotten. Without `--state-file` a restart loses everything (the
  ledger's own retention bounds memory, not durability).
- **Payouts are v1.** `--payout-threshold/--payout-interval` drive a
  `sendtoaddress` loop from the node wallet (dry-run unless
  `--payout-exec`); worker names double as payout addresses. Not yet
  tracked: coinbase maturity/confirmations (payouts execute on accept —
  sends fail safe with "insufficient funds" while coinbases mature, so
  run a mature chain or lower expectations), fee splits beyond the
  per-block skim. The payout ledger itself now survives restarts with
  `--state-file`; `payout_totals` in `/api/stats` reconciles it
  (coinbases == owed + paid + fees).
- **No auth, no TLS.** Stratum and the `--http` dashboard are plaintext
  and identify workers by name hash; bind to localhost or a private
  network.
- **Claim modes change block liveness.** `--claim-jackpots` /
  `--claim-pot` serve claim templates and locally withhold non-drawing
  finds (1/100 and 1/1000 draws). That is correct for hunting a claim,
  but it throttles block submission — don't run a public pool that way.
- **Work-identity dedupe is windowed** (2^16 identities). A replay older
  than the window can pass; accepted as the reference trade-off.
- **Stratum V1 is the default wire; V2 serves alongside it** behind flags
  (pool `--v2-bind [--v2-authority-key HEX --v2-cert-validity SECS]`,
  miner `pool --v2 --v2-authority URLKEY|HEX`): Noise NX handshake with
  authority pinning, `SetupConnection` negotiation, one standard channel
  per connection, `LuckyJob` full-template dispatch and `LuckySubmit`
  (u64 nonce) validation through the same registry/dedupe/ledger as V1.
  V2 auth reuses the V1 password/token map (token rides as device id);
  vardiff mirrors V1 per channel (core `SetTarget` + same-header retarget
  jobs; templates reset to the template target). The provider
  seam and money rules carry over. No `set_difficulty` — targets ride in `mining.notify`.
  Wire compatibility is proven by an interop test against `codec_sv2`
  7.0.0 (dev-dependency): a reference handshake against our session,
  both transport directions byte-identical, length tables agreeing.

Tracked follow-ups: persistence + payout processor + auth
(`internal/17`), V2 multi-channel + interop test against
`codec_sv2`'s Encoder/Decoder,
`lm-miner pool` subcommand speaking
`client`, attestation-gated league entry.
