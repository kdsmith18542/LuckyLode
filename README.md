# Lucky Lode

[![CI](https://github.com/kdsmith18542/LuckyLode/actions/workflows/ci.yml/badge.svg)](https://github.com/kdsmith18542/LuckyLode/actions/workflows/ci.yml)
[![License: GPL-3.0](https://img.shields.io/badge/License-GPL--3.0--only-blue.svg)](LICENSE)

A fair-launch, hybrid PoW/PoS mineable coin for everyday hardware —
phones, laptops, desktops, and GPUs — where every PoW block carries a
pre-committed lucky draw (jackpot, Lucky Pot, Luckynode Draw) on top of
the block reward. No premine, no team allocation: the only genesis
seeds are consensus-locked game funds that pay miners and draw winners.
Website: **<https://luckylode.cc>**

**Status: public testnet.** Coins are `tLKYL` and worthless by design —
the point is to break things before mainnet. Live services:
[pool](https://luckylode.cc/pool/) (stratum `luckylode.cc:3346`),
[explorer](https://luckylode.cc/explorer/),
[faucet](https://luckylode.cc/faucet/).

New here? Read [the coin at a glance](docs/00-lucky-lode-at-a-glance.md)
— supply, consensus, the game layer, and current status on one page.
The full design record lives in [`docs/`](docs/README.md).
The user wiki ([`wiki/`](wiki/Home.md), [rendered here](https://luckylode.cc/wiki/))
covers the wallet, mining, and luckynodes from the user's side.

## Try it in five minutes (testnet)

1. Get an address: build the desktop wallet ([`lm-wallet/`](lm-wallet/README.md))
   and copy a receive address — or `luckyd -testnet getnewaddress`.
2. Fund it: paste the address into the [faucet](https://luckylode.cc/faucet/).
3. Mine to the official pool (pays to your address, no account needed):

```sh
cargo run -p lm-miner -- pool --pool luckylode.cc:3346 \
  --worker <your tLKYL address> --device auto
```

4. Watch your shares land on the [pool dashboard](https://luckylode.cc/pool/)
   and your blocks in the [explorer](https://luckylode.cc/explorer/).
   Solo instead? See [Solo mining](guides/solo-mining.md).

## Repository layout

| Repository | Contents |
|---|---|
| [LuckyLode](https://github.com/kdsmith18542/LuckyLode) (this repo) | Rust reference software: miner, pool, wallet core + desktop wallet, explorer, faucet, monitoring; design documents |
| [LuckyLode-node](https://github.com/kdsmith18542/LuckyLode-node) | The consensus node — a Verus v1.2.18 fork with LuckyHash PoW, the Lucky Pot and draw rules, and Lucky Lode chain parameters |

Crates: `lm-pow` (LuckyHash + exact difficulty + jackpot trigger),
`lm-rewards` (game money rules), `lm-pool` (PPLNS ledger, divisions,
payouts), `lm-stratum` (Stratum V1 + V2 pool wire layer), `lm-miner`
(CPU/OpenCL miner), `lm-keys` (self-custody wallet core),
`lm-luckynode` (consensus Luckynode Draw participation: on-chain
identity registration + heartbeats), `lm-explorer`, `lm-sync`,
`lm-sched` (thermal policy), `lm-mobile` (Android miner), plus the
Tauri desktop wallet in `lm-wallet/`.

## Build and test

```sh
cargo test --workspace                 # Rust reference software
cargo clippy --workspace --all-targets -- -D warnings
(cd lm-wallet/src-tauri && cargo test) # wallet backend (separate workspace)
```

`cargo test --workspace` covers every `lm-*` crate; the wallet is a
separate Tauri workspace by design (GUI builds are heavy and
platform-specific). Full dev setup — node build env, wallet
(`npm`/Tauri), Android SDK/NDK — is in [CONTRIBUTING.md](CONTRIBUTING.md).

To run the node, see the
[LuckyLode-node README](https://github.com/kdsmith18542/LuckyLode-node#readme);
to mine or run a pool, see the guides:

- [Solo mining](guides/solo-mining.md)
- [Running a neutral pool](guides/pool-neutrality.md)
- [Pool and explorer API reference](guides/api.md)

## Contributing and security

- Contributions: [CONTRIBUTING.md](CONTRIBUTING.md)
- Vulnerabilities: [SECURITY.md](SECURITY.md) — please report privately

## License

Code is **GPL-3.0-only** ([LICENSE](LICENSE)); design documents are
CC-BY-SA-4.0; the node fork inherits upstream MIT with notices
retained (see the NOTICE files in both repositories).
