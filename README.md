# Lucky Lode

[![CI](https://github.com/kdsmith18542/LuckyLode/actions/workflows/ci.yml/badge.svg)](https://github.com/kdsmith18542/LuckyLode/actions/workflows/ci.yml)
[![License: GPL-3.0](https://img.shields.io/badge/License-GPL--3.0--only-blue.svg)](LICENSE)

A fair-launch, hybrid PoW/PoS mineable coin with masternodes for the lower end hardware and gpus — miniPCs, laptops old and new — with mining
designed as a exciting Lcuky adventure. Website: **<https://luckylode.cc>**

New here? Read [the coin at a glance](docs/00-lucky-lode-at-a-glance.md)
— supply, consensus, the game layer, and current status on one page.
The full design record lives in [`docs/`](docs/README.md).

## Repository layout

| Repository | Contents |
|---|---|
| [LuckyLode](https://github.com/kdsmith18542/LuckyLode) (this repo) | Rust reference software: miner, pool, wallet core + desktop wallet, explorer, faucet, monitoring; design documents |
| [LuckyLode-node](https://github.com/kdsmith18542/LuckyLode-node) | The consensus node — a Verus v1.2.18 fork with LuckyHash PoW, the Lucky Pot and draw rules, and Lucky Lode chain parameters |

Crates: `lm-pow` (LuckyHash + exact difficulty + jackpot trigger),
`lm-rewards` (game money rules), `lm-pool` (PPLNS ledger, leagues,
payouts), `lm-stratum` (Stratum V1 + V2 pool wire layer), `lm-miner`
(CPU/OpenCL miner), `lm-keys` (self-custody wallet core),
`lm-luckynode` (bonded staker registry), `lm-explorer`, `lm-sync`,
`lm-sched` (thermal policy), plus the Tauri desktop wallet in
`lm-wallet/`.

## Build and test

```sh
cargo test --workspace                 # full suite, all crates
cargo clippy --workspace --all-targets -- -D warnings
```

To run the node, see the
[LuckyLode-node README](https://github.com/kdsmith18542/LuckyLode-node#readme);
to mine or run a pool, see the guides:

- [Solo mining](guides/solo-mining.md)
- [Running a neutral pool](guides/pool-neutrality.md)

## Contributing and security

- Contributions: [CONTRIBUTING.md](CONTRIBUTING.md)
- Vulnerabilities: [SECURITY.md](SECURITY.md) — please report privately

## License

Code is **GPL-3.0-only** ([LICENSE](LICENSE)); design documents are
CC-BY-SA-4.0; the node fork inherits upstream MIT with notices
retained (see the NOTICE files in both repositories).
