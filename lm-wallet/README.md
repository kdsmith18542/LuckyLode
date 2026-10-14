# Lucky Lode Wallet (desktop)

Tauri v2 desktop wallet: wallet core, **luckynodes** (registry, bond,
embedded heartbeat/cosign service), and all game/network stats — spec in
`internal/19-wallet-gui.md`.

## Layout

- `ui/` — Svelte 5 + Vite + TypeScript frontend (design tokens match the
  explorer/pool/website: dark, gold, green).
- `src-tauri/` — Rust backend (standalone cargo project):
  - `node.rs` — node JSON-RPC (reuses `lm-stratum::rpc::RpcClient`)
  - `pool.rs` — pool HTTP API (std-only)
  - `draws.rs` — pre-committed draw math (mirrors `lucky_draw.h`; KAT
    vectors pinned in tests)
  - `luckynode.rs` — embedded heartbeat + draw-cosign service
  - `commands.rs` — the Tauri command surface

## Develop

```sh
cd lm-wallet/ui && npm install
cd ../src-tauri && cargo tauri dev   # or: cargo run (after ui is built)
```

## Build

```sh
cd lm-wallet/ui && npm run build
cd ../src-tauri && cargo build --release
```

Bundling (`deb`/AppImage/msi/dmg) is disabled until icons and signing
are set up (`tauri.conf.json` → `bundle.active`).

## Defaults

- Node RPC: `http://127.0.0.1:9271` (testnet port; Settings tab).
- Pool API: `https://luckylode.cc/pool`.
- The luckynode ed25519 key is stored 0600 in the app config dir; it is
  separate from wallet funds and never leaves the machine.

## Status (M1)

Built: shell + status bar, next-block draw banner (pre-committed draws),
overview balances/activity, send/receive, luckynode list + register +
embedded service, game stats, settings. Next (M2–M5 in the spec):
contacts/QR, draw history polish, luck chart, notifications, themes,
packaging, OS keyring.
