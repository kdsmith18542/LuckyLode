# Solo mining Lucky Lode

**Status:** testnet guide. Solo mining is fully supported from day one —
it uses the same open-source stack as pools, run locally. You keep every
coin the chain pays you: block reward, fees, and any jackpot or pot
bonus. (Luckynode Draw claims ride along in the coinbase but pay the
draw winner, not you — consensus money, doc 06 §6.7.) No fee, no split,
no third party.

## What "solo" means here

Three solo paths exist:

0. **The Lucky Lode wallet** (easiest) — Miner tab → **solo** mode:
   the wallet grinds getblocktemplate jobs from your own node with
   claim-aware coinbases, no pool in the loop. Rewards land wherever the
   node's coinbase pays, so the wallet refuses to start unless that is
   one of your addresses — point the node at your wallet first (Miner
   tab → *point node payouts at my wallet*, then restart the node), or
   start the managed node with `-mineraddress=<YOUR-m-ADDRESS>
   -minetolocalwallet=0` (testnet `m`/`n` prefixes — see below).
1. **The node's own miner** (`luckyd -gen=1`, or `generate` on a local
   node) — claim-aware since 2026-10-21: on jackpot/pot draw heights the
   coinbase claims the exact bonus/splash for the miner. Point
   `-mineraddress` at your address. Full parity with pool mining, no
   extra software.
2. **The reference stack** — you run all three pieces yourself (below);
   useful when you want vardiff, dashboards, or several workers.

Either way you keep every coin the chain pays you: block reward, fees,
and any jackpot or pot bonus. No fee, no split, no third party.

1. **Node** (`luckyd`) — holds the chain, builds block templates, pays the
   coinbase to *your* address.
2. **Reference pool** (`pool_server`, localhost-only) — bridges the miner's
   Stratum to the node's `getblocktemplate`, submits found blocks.
3. **Miner** (`lm-miner`) — hashes against your local pool.

The pool software takes no cut by default (`--pool-fee-pp` defaults to 0),
and with a single worker there is nobody to split with. The jackpot bonus
is paid in the coinbase to the block finder — you.

## Requirements

- `luckyd` + `pool_server` + `lm-miner` binaries (build from source;
  see the repo README).
- A Lucky Lode receiving address (create one in the node wallet with
  `getnewaddress`; it starts with `L` on mainnet, `m`/`n` on testnet).
- Any hardware that runs the miner: miniPC, laptop, desktop, low-end GPU.
  See the device matrix (doc 05 §5.6) for expected rates.
- A synced node (let it catch up to the tip before mining).

## The three commands

First create the node's config (the pool logs in with these — pick
your own values):

```sh
# ~/.luckylodetest/LUCKYLODETEST.conf
rpcuser=<pick-a-username>
rpcpassword=<pick-a-long-password>
```

**1. Node** — the coinbase pays `-mineraddress` (your testnet address):

```sh
./luckyd -ac_algo=luckyhash -testnet -datadir=$HOME/.luckylodetest \
  -mineraddress=<YOUR-m-ADDRESS> -daemon -listen=1
```

Without `-testnet` (and the testnet datadir) you would start a
mainnet-params node that cannot talk to the pool below — and `-testnet`
binds RPC to `127.0.0.1:42253` by default, which is what step 2
assumes.

**2. Local pool** — node mode against your node; claims are automatic
(jackpot/pot/prize from the node's exact advertisements — every find
lands; `--no-claim-*` opts out). The pool splices the claim amounts
into the coinbase and validates nothing itself: a template the node
would refuse is a loud rejected block, never silent bad money:

```sh
./pool_server --node-rpc 127.0.0.1:42253 --rpc-user <U> --rpc-pass <P> \
  --bind 127.0.0.1:3344 \
  --payout-threshold 0 --payout-interval 60 --payout-exec
```

`<U>`/`<P>` are the `rpcuser`/`rpcpassword` from the node config
above; `42253` is the flagless `-testnet` RPC port.

Claims are automatic (jackpot/pot/prize/faucet/dev from the node's
exact advertisements — no `--claim-*` flags needed; `--no-claim-*`
opts out). Faucet/dev legs are pool-claimed seed drips, not miner
income; the prize claim pays the draw winner (consensus money), never
you — jackpot + pot are yours in full.
`--payout-threshold 0 --payout-exec` pays every balance straight through
(no dust held back). Omit `--payout-exec` for a dry run that only logs.

**3. Miner** — pointed at your pool, worker name = your payout address:

```sh
./lm-miner pool --pool 127.0.0.1:3344 --worker <YOUR-m-ADDRESS> \
  --threads <N> --max-temp 80 --duty 50
```

Bind the pool to localhost (`127.0.0.1`) unless you know why you're
exposing it. Never expose the node's RPC to the network.

## Honest expectations (read before soloing)

- **Variance is brutal.** Solo, you earn the whole block or nothing. At
  phone/miniPC hashrate on a live network, dry spells of weeks are normal.
  Pools exist to smooth this — same coins, steadier drip. Solo is for the
  romance and the hall of fame (doc 03 §3.5), not for steady income.
- **Jackpots are yours alone.** A solo jackpot pays the full
  `min(5R, reserve)` bonus to your coinbase — no sqrt-split, no cap-share.
  That's the upside that makes solo rational for jackpot hunters.
- **Maturity applies.** Fresh coinbases can't be spent (or staked) until
  they mature — don't panic when a found block isn't spendable yet.

## Troubleshooting

- `unknown job` / stale shares: your node fell behind — check `getblockcount`
  against the explorer, wait for sync.
- `bad-cb-amount`: template/claim mismatch — on regtest, `--pot-divisor /
  --jackpot-divisor` must match the node's `-lucky-*-divisor` overrides
  (testnet/mainnet read them from the node automatically).
- Port conflicts: a second `luckyd` silently fails to bind while the old one
  answers — verify with `pgrep -x luckyd` and check the height.
- Always stop the node gracefully (`stop` via RPC); `kill -9` can lose
  unflushed blocks.
