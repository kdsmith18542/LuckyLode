# Lucky Lode Wallet (desktop)

Tauri v2 desktop wallet: wallet core, **luckynodes** (registry, bond,
embedded heartbeat/cosign service), and all game/network stats — spec in
`-wallet-gui.md`.

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

Bundles (`deb` + AppImage) are enabled (`tauri.conf.json` →
`bundle.active`):

```sh
cd lm-wallet/src-tauri && cargo tauri build --bundles deb,appimage
```

AppImage note: linuxdeploy scans `$PATH` and aborts on unreadable
entries. A dangling `/usr/sbin/poetry` → `/root/.local/bin/poetry`
symlink breaks the build on this box — trim the `sbin` dirs:

```sh
cd lm-wallet/src-tauri
PATH=$(echo "$PATH" | tr ':' '\n' | grep -vE '/sbin$' | paste -sd:) \
  cargo tauri build --bundles appimage
```

Verified click-to-run: the built AppImage launches standalone (bundles
webkit/gtk; needs `libfuse2`).

## Defaults

- Node connection is preset-based (Settings → Node): **Shared testnet
  node** (`https://luckylode.cc/node/`, public `u`/`p` testnet creds,
  embedded by the app — nobody types them) or **Node on this computer**
  (the wallet manages the node daemon: auto-detects luckyd on PATH and the
  usual per-OS install locations, resolves the daemon's own default
  datadir per platform, and generates its own RPC credentials to pass
  to the node — the user never sees them; RPC is pinned to loopback).
  A fresh install points at `127.0.0.1:42253` (local-first, like Core;
  the flagless testnet RPC default, verified on a clean machine)
  and the top bar suggests the shared node until one of them is running.
  Advanced exposes the raw RPC URL/user/password and node location for
  custom setups — `http://`, `https://`, and URL path prefixes are all
  supported.
- Pool API: `https://luckylode.cc/pool`.
- The luckynode ed25519 key is stored 0600 in the app config dir; it is
  separate from wallet funds and never leaves the machine.

## Status (M1+)

Built: shell + status bar, next-block draw banner (pre-committed draws),
overview balances/activity (**per-tx detail**), send/receive (**contacts
address book + receive QR**), **embedded miner** (Miner tab:
CPU/OpenCL — GPU device picker, batch (MiB VRAM), NVIDIA power limit,
duty + temperature guard, live hashrate/shares/temp/power/log — same
engine as the reference `lm-miner`; live-verified CPU 846 H/s and GPU
10.9 kH/s with accepted shares), luckynode list + register + embedded
service, game stats (**live emission phase**, **luck chart**, draws,
league read-only), settings (**dark/light theme**). Packaged: deb +
AppImage (click-to-run verified). Next (M3–M5 in the spec): luckynode
wizard, OS notifications, FAQ, OS keyring.
