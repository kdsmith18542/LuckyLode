# Changelog

All notable changes to the Lucky Lode reference software. The project is
pre-1.0; minor versions may change APIs.

## Unreleased

### Added

- Explorer: paginated block index (`/blocks`, `/api/blocks`), block
  prev/next navigation, search on every detail page, address history
  paging, Prize Fund + current Luckynode Draw on the rewards page,
  outage/staleness banners, per-page titles, SVG favicon, node-derived
  ticker.
- Pool dashboard: current-epoch draw card, open-pool "My miner" lookup,
  URL-hash tabs, connecting/stale states, mobile table scrolling.
- Wallet: lock-screen recovery from the backup phrase, first-run
  create/restore tabs, two-step review for registration and key import,
  persistent Miner settings, tab-independent notifications, shared
  number formatters, accessibility labels/focus/keyboard rows, loading
  states.
- `--version` on every binary; per-binary CLI contract tests.
- Contributor setup for node, wallet, Android and a local regtest stack;
  pool/explorer API reference (`guides/api.md`); READMEs for every crate.

### Changed

- Pool: `--mock` is now required for the mock chain; startup prints
  chain/auth/state; SIGINT saves state like SIGTERM; fee percentages
  above 100% are rejected.
- Faucet and the luckynode daemon refuse non-testnet chains without
  `--allow-mainnet`; their state files fail closed when corrupt.
- Luckynode daemon state writes are atomic; duplicate heartbeats on an
  idle chain are suppressed.
- `lm-sync` rejects unknown flags/non-numeric values and refuses
  equal-height divergent pulls.
- Explorer supply/node views are cached off the index lock; the emission
  schedule and base-unit formatting live once in `lm-rewards`.
- CPU-only miner builds (`--no-default-features`) work again; CI covers
  the wallet and mobile facade.
- CI: wallet job (tests + clippy + frontend build), CPU-only and
  `lm-mobile` checks.

### Fixed

- Pool dashboard: stored XSS via worker names/agents (escaping was a
  no-op), duplicate payouts table, footer markup, payout badge.
- Explorer: quote-safe escaping, real 404 statuses/JSON errors, search
  redirects, negative amounts losing their sign, mainnet ticker
  mislabelling.
- Wallet: pot tier shown as a percent (87 vs the 25% band), previews
  editable after review, lock-screen dead end, own-send change
  announced as a payout.
- Consensus docs: testnet RPC port, draw seed/claim epoch math,
  `<=` target comparison, four-seed fair-launch wording.
- Mutex poisoning could cascade through pool/wallet threads (206 lock
  sites now recover instead of panicking).

### Removed

- Dead bond-era pool helpers and coinbase-script derivation.
- Stale bond/cosign/league language across docs, wiki and crate READMEs.

## v0.1.0 — initial testnet software

First tagged snapshot of the reference stack: LuckyHash PoW, LWMA
difficulty, jackpot/Lucky Pot/prize-fund rules, Stratum V1+V2 pool,
CPU/OpenCL miner, desktop wallet, explorer, faucet, monitors and the
Android miner (0.2.0 APK).
